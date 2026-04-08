/**
 * Producer mesh bootstrap -- founding node + join.
 *
 * Spec reference: Aster-trust-spec.md S2.1, S2.5.  Plan: ASTER_PLAN.md S14.5.
 *
 * Two startup modes:
 *
 * startFoundingNode()
 *     The first producer in a new mesh. Generates a random 32-byte salt, derives
 *     the gossip topic, initializes MeshState, and returns it for the caller.
 *
 * joinMesh()
 *     A subsequent producer. Builds an AdmissionRequest from its credential.
 *     The caller dials the bootstrap peer and sends the request, then calls
 *     applyAdmissionResponse() with the result.
 *
 * handleAdmissionRpc()
 *     Server-side handler: parses a request, runs offline admission checks,
 *     and returns an AdmissionResponse.
 */

import { readFileSync, writeFileSync, existsSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { homedir } from 'node:os';
import { randomBytes } from 'node:crypto';

import { MeshState, saveMeshState } from './mesh.js';
import { deriveGossipTopic } from './gossip.js';
import type { EnrollmentCredential } from './credentials.js';
import { hexToBytes, bytesToHex } from './credentials.js';
import { checkOffline } from './admission.js';

// ── Types ───────────────────────────────────────────────────────────────────

export interface BootstrapConfig {
  /** State directory (default: ~/.aster). */
  stateDir?: string;
}

export interface AdmissionRequest {
  credentialJson: string;
  iidToken?: string;
}

export interface AdmissionResponse {
  accepted: boolean;
  salt: string;           // hex-encoded 32-byte salt
  acceptedProducers: string[];
  /** Internal only -- not exposed to peer on the wire. */
  reason?: string;
}

// ── Internal helpers ────────────────────────────────────────────────────────

const DEFAULT_STATE_DIR = join(homedir(), '.aster');

function stateDir(config?: BootstrapConfig): string {
  return config?.stateDir ?? process.env.ASTER_MESH_STATE_DIR ?? DEFAULT_STATE_DIR;
}

function statePath(name: string, config?: BootstrapConfig): string {
  return join(stateDir(config), name);
}

function ensureStateDir(config?: BootstrapConfig): void {
  const dir = stateDir(config);
  if (!existsSync(dir)) {
    mkdirSync(dir, { recursive: true });
  }
}

/**
 * Load an EnrollmentCredential from a JSON file.
 * Path resolved from argument -> ASTER_ENROLLMENT env var.
 */
function loadEnrollmentCredential(path?: string): EnrollmentCredential {
  const envPath = path ?? process.env.ASTER_ENROLLMENT;
  if (!envPath) {
    throw new Error(
      'Set ASTER_ENROLLMENT to the path of your enrollment credential JSON file',
    );
  }
  const raw = readFileSync(envPath, 'utf-8');
  const d = JSON.parse(raw);
  return {
    endpointId: d.endpoint_id,
    rootPubkey: d.root_pubkey,   // already hex
    expiresAt: Number(d.expires_at),
    attributes: d.attributes ?? {},
    signature: d.signature ?? '',
  };
}

/**
 * Load or generate 32-byte mesh salt.
 * Persists to stateDir/mesh_salt for crash recovery.
 */
function loadOrGenerateSalt(config?: BootstrapConfig): Uint8Array {
  const saltPath = statePath('mesh_salt', config);
  ensureStateDir(config);
  if (existsSync(saltPath)) {
    const buf = readFileSync(saltPath);
    if (buf.length !== 32) {
      throw new Error(`mesh_salt at ${saltPath} is ${buf.length} bytes; expected 32`);
    }
    return new Uint8Array(buf);
  }
  const salt = randomBytes(32);
  writeFileSync(saltPath, salt);
  return new Uint8Array(salt);
}

/**
 * Persist a MeshState to stateDir/mesh_state.json (atomic rename).
 */
function persistMeshState(state: MeshState, config?: BootstrapConfig): void {
  const path = statePath('mesh_state.json', config);
  saveMeshState(state, path);
}

// ── Founding node ───────────────────────────────────────────────────────────

/**
 * Start the founding node of a new producer mesh.
 *
 * Steps (S2.1):
 * 1. Load credential from JSON file.
 * 2. Verify credential offline.
 * 3. Generate or load 32-byte salt.
 * 4. Derive gossip topic.
 * 5. Create MeshState with self as only accepted producer.
 * 6. Persist state.
 *
 * @returns The initialized MeshState.
 */
export async function startFoundingNode(
  enrollmentPath: string,
  config?: BootstrapConfig,
): Promise<MeshState> {
  // 1. Load credential
  const cred = loadEnrollmentCredential(enrollmentPath);

  // 2. Verify offline
  const result = await checkOffline(cred, cred.endpointId);
  if (!result.admitted) {
    throw new Error(`Founding node credential invalid: ${result.reason}`);
  }

  // 3. Salt
  const salt = loadOrGenerateSalt(config);

  // 4. Topic derivation
  await deriveGossipTopic(hexToBytes(cred.rootPubkey), salt); // validates topic derivation

  // 5. MeshState -- self is the only accepted producer
  const state = new MeshState();
  state.addPeer(cred.endpointId);

  // 6. Persist
  persistMeshState(state, config);

  return state;
}

// ── Join mesh ───────────────────────────────────────────────────────────────

/**
 * Build an AdmissionRequest from a credential for joining an existing mesh.
 *
 * The caller should send this request to the bootstrap peer over
 * the aster.producer_admission ALPN, then call applyAdmissionResponse()
 * with the result.
 */
export function joinMesh(
  credential: EnrollmentCredential,
  iidToken?: string,
): AdmissionRequest {
  const credJson = JSON.stringify({
    endpoint_id: credential.endpointId,
    root_pubkey: credential.rootPubkey,
    expires_at: credential.expiresAt,
    attributes: credential.attributes,
    signature: credential.signature,
  });
  return {
    credentialJson: credJson,
    iidToken,
  };
}

// ── Apply admission response ────────────────────────────────────────────────

/**
 * Finalize MeshState after receiving a successful AdmissionResponse.
 *
 * @param response        The AdmissionResponse from the bootstrap peer.
 * @param ownEndpointId   This node's endpoint ID.
 * @param rootPubkey      The root public key (raw bytes) for topic derivation.
 * @returns Initialized MeshState ready for gossip subscription.
 * @throws If response.accepted is false.
 */
export async function applyAdmissionResponse(
  response: AdmissionResponse,
  ownEndpointId: string,
  rootPubkey: Uint8Array,
): Promise<MeshState> {
  if (!response.accepted) {
    throw new Error(
      `Admission refused: ${response.reason ?? '(no reason provided)'}`,
    );
  }

  const salt = hexToBytes(response.salt);
  await deriveGossipTopic(rootPubkey, salt); // validates topic derivation

  const state = new MeshState();
  // Add all accepted producers + self
  for (const peerId of response.acceptedProducers) {
    state.addPeer(peerId);
  }
  state.addPeer(ownEndpointId);

  return state;
}

// ── Server-side admission RPC ───────────────────────────────────────────────

/**
 * Server-side handler for aster.producer_admission ALPN.
 *
 * Parses an AdmissionRequest, runs offline admission checks, and returns
 * an AdmissionResponse. On success, the peer is added to ownState.
 *
 * @param requestJson     JSON-serialized credential (the credentialJson field
 *                        from AdmissionRequest, or a raw credential JSON).
 * @param ownState        The founding/accepting node's MeshState.
 * @param ownRootPubkey   Hex-encoded root public key this mesh trusts.
 * @param config          Optional BootstrapConfig.
 * @returns AdmissionResponse (accepted or rejected with reason).
 */
export async function handleAdmissionRpc(
  requestJson: string,
  ownState: MeshState,
  ownRootPubkey: string,
  config?: BootstrapConfig,
): Promise<AdmissionResponse> {
  let cred: EnrollmentCredential;
  try {
    const d = JSON.parse(requestJson);
    cred = {
      endpointId: d.endpoint_id,
      rootPubkey: d.root_pubkey,
      expiresAt: Number(d.expires_at),
      attributes: d.attributes ?? {},
      signature: d.signature ?? '',
    };
  } catch {
    return {
      accepted: false,
      salt: '',
      acceptedProducers: [],
      reason: 'malformed request',
    };
  }

  // Verify the credential's root_pubkey matches the mesh's trusted key
  if (cred.rootPubkey !== ownRootPubkey) {
    return {
      accepted: false,
      salt: '',
      acceptedProducers: [],
      reason: 'untrusted root key',
    };
  }

  // Run offline admission checks (signature, expiry, endpoint_id match)
  const result = await checkOffline(cred, cred.endpointId);
  if (!result.admitted) {
    return {
      accepted: false,
      salt: '',
      acceptedProducers: [],
      reason: result.reason ?? 'admission check failed',
    };
  }

  // Accept: add peer to mesh state
  ownState.addPeer(cred.endpointId);
  persistMeshState(ownState, config);

  // Load salt from state dir for response
  let saltHex = '';
  try {
    const saltPath = statePath('mesh_salt', config);
    if (existsSync(saltPath)) {
      saltHex = bytesToHex(new Uint8Array(readFileSync(saltPath)));
    }
  } catch {
    // Salt unavailable -- ephemeral mesh
  }

  return {
    accepted: true,
    salt: saltHex,
    acceptedProducers: ownState.allPeers(),
    reason: '',  // never leak reason on wire
  };
}

// ── Per-connection handler ──────────────────────────────────────────────────

/**
 * Handle one producer admission connection: read request, write response.
 *
 * @param conn            An IrohConnection-like object with acceptBi() and remoteId().
 * @param ownRootPubkey   Hex-encoded root public key.
 * @param ownState        This node's MeshState; mutated on accept.
 * @param config          Optional BootstrapConfig.
 * @returns The AdmissionResponse that was sent.
 */
export async function handleProducerAdmissionConnection(
  conn: {
    remoteId(): string;
    acceptBi(): Promise<[{ writeAll(data: Uint8Array): Promise<void>; finish(): Promise<void> }, { readToEnd(maxBytes: number): Promise<Uint8Array> }]>;
  },
  ownRootPubkey: string,
  ownState: MeshState,
  config?: BootstrapConfig,
): Promise<AdmissionResponse> {
  const peerId = conn.remoteId();
  try {
    const [send, recv] = await conn.acceptBi();
    const raw = await recv.readToEnd(64 * 1024);
    if (raw.length === 0) {
      const resp: AdmissionResponse = {
        accepted: false,
        salt: '',
        acceptedProducers: [],
        reason: 'empty request',
      };
      return resp;
    }

    // Parse the AdmissionRequest wrapper and extract credential_json
    let credJson: string;
    try {
      const wrapper = JSON.parse(new TextDecoder().decode(raw));
      credJson = wrapper.credential_json ?? '';
    } catch {
      // Back-compat: accept raw credential JSON as well
      credJson = new TextDecoder().decode(raw);
    }

    const response = await handleAdmissionRpc(credJson, ownState, ownRootPubkey, config);

    // Write response (strip reason -- oracle protection)
    const wirePayload = {
      accepted: response.accepted,
      salt: response.salt,
      accepted_producers: response.acceptedProducers,
      reason: '',  // oracle protection -- never leak on wire
    };
    await send.writeAll(new TextEncoder().encode(JSON.stringify(wirePayload)));
    await send.finish();

    return response;
  } catch (exc) {
    return {
      accepted: false,
      salt: '',
      acceptedProducers: [],
      reason: `connection error from ${peerId}: ${exc}`,
    };
  }
}

// ── Ephemeral state ─────────────────────────────────────────────────────────

/**
 * Build an in-memory MeshState for a standalone producer.
 *
 * Useful for demos, tests, and single-node setups. Generates a fresh random
 * salt and an empty accepted-producer set with no persistence.
 */
export async function makeEphemeralMeshState(rootPubkey?: Uint8Array): Promise<MeshState> {
  const salt = randomBytes(32);
  const state = new MeshState();
  // Derive topic if rootPubkey provided (for gossip subscription)
  if (rootPubkey != null) {
    const topicId = await deriveGossipTopic(rootPubkey, new Uint8Array(salt));
    state.topicId = bytesToHex(topicId);
  }
  return state;
}
