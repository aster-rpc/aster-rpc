/**
 * Producer mesh gossip: signing, verification, message handling, heartbeat.
 *
 * Spec reference: Aster-trust-spec.md S2.3, S2.6.  Plan: ASTER_PLAN.md S14.2-14.6.
 *
 * NOT to be confused with registry/gossip.ts which handles service discovery gossip.
 * This module handles the producer mesh gossip protocol -- signing, verification,
 * message dispatch, and lease heartbeat.
 *
 * Canonical signing bytes (normative -- S2.6, ASTER_PLAN.md S14.2):
 *
 *     u8(type) || payload || sender.encode('utf-8') || u64_be(epoch_ms)
 *
 * Do NOT reorder epoch_ms before sender+payload -- the spec fixes this
 * byte order and any deviation breaks cross-implementation verification.
 *
 * Topic derivation (S2.3):
 *
 *     blake3(root_pubkey + "aster-producer-mesh" + salt).digest().slice(0, 32)
 */

import { sign, verify, concatBytes, bytesToHex } from './credentials.js';
import { validateRcan } from './rcan.js';
import type { MeshState } from './mesh.js';
import type { ClockDriftTracker } from './clock.js';

// ── Types ───────────────────────────────────────────────────────────────────

export interface ProducerMessage {
  type: number;
  payload: Uint8Array;
  sender: string;       // endpoint ID hex
  epochMs: number;
  signature: Uint8Array; // 64 bytes
}

export enum ProducerMessageType {
  INTRODUCE = 1,
  DEPART = 2,
  CONTRACT_PUBLISHED = 3,
  LEASE_UPDATE = 4,
}

export interface ContractPublishedPayload {
  serviceName: string;
  version: number;
  contractCollectionHash: string;
}

export interface LeaseUpdatePayload {
  serviceName: string;
  version: number;
  contractId: string;
  healthStatus: string;
  addressingInfo: Record<string, string>;
}

export interface HandleMessageOptions {
  state: MeshState;
  config: { replayWindowMs: number };
  peerPubkeys: Map<string, Uint8Array>;  // endpoint_id -> pubkey
  registryCallback?: (eventType: string, payload: ContractPublishedPayload | LeaseUpdatePayload) => void;
  driftTracker?: ClockDriftTracker;
  onSelfDeparture?: () => void;
  logger?: { info(...args: unknown[]): void; warn(...args: unknown[]): void; debug(...args: unknown[]): void; error(...args: unknown[]): void };
}

/** Maximum gossip payload size (matches Python aster.limits.MAX_GOSSIP_PAYLOAD_SIZE). */
const MAX_GOSSIP_PAYLOAD_SIZE = 64 * 1024;

// ── Topic derivation ────────────────────────────────────────────────────────

/**
 * Derive the 32-byte gossip topic for the producer mesh.
 *
 * blake3(root_pubkey + "aster-producer-mesh" + salt).slice(0, 32)
 *
 * Falls back to sha256 if blake3 is not available.
 */
export async function deriveGossipTopic(rootPubkey: Uint8Array, salt: Uint8Array): Promise<Uint8Array> {
  const label = new TextEncoder().encode('aster-producer-mesh');
  const data = concatBytes([rootPubkey, label, salt]);

  try {
    // Try @noble/hashes blake3 first
    // @ts-ignore — optional dependency
    const { blake3 } = await import('@noble/hashes/blake3') as any;
    return blake3(data, { dkLen: 32 });
  } catch {
    // Fallback: sha256 via node:crypto (note: Python uses blake3)
    const { createHash } = await import('node:crypto');
    const hash = createHash('sha256').update(data).digest();
    return new Uint8Array(hash.buffer, hash.byteOffset, 32);
  }
}

// ── Canonical signing bytes ─────────────────────────────────────────────────

/**
 * Return the canonical bytes that are signed / verified.
 *
 * Normative byte order (S2.6):
 *     u8(type) || payload || sender.encode('utf-8') || u64_be(epoch_ms)
 */
export function producerMessageSigningBytes(
  msgType: number,
  payload: Uint8Array,
  sender: string,
  epochMs: number,
): Uint8Array {
  const typeByte = new Uint8Array([msgType & 0xff]);
  const senderBytes = new TextEncoder().encode(sender);
  const epochBytes = writeU64BE(epochMs);
  return concatBytes([typeByte, payload, senderBytes, epochBytes]);
}

/** u64 big-endian encoding. */
function writeU64BE(value: number): Uint8Array {
  const buf = new ArrayBuffer(8);
  const view = new DataView(buf);
  view.setUint32(0, Math.floor(value / 0x100000000));
  view.setUint32(4, value >>> 0);
  return new Uint8Array(buf);
}

// ── Sign / verify ───────────────────────────────────────────────────────────

/**
 * Create and sign a ProducerMessage.
 *
 * @param msgType        ProducerMessageType integer value.
 * @param payload        Serialized per-type payload bytes.
 * @param sender         This node's endpoint_id (hex string).
 * @param epochMs        Wall-clock timestamp in milliseconds.
 * @param signingKeyRaw  32-byte raw ed25519 private key seed.
 * @returns A fully signed ProducerMessage.
 */
export async function signProducerMessage(
  msgType: number,
  payload: Uint8Array,
  sender: string,
  epochMs: number,
  signingKeyRaw: Uint8Array,
): Promise<ProducerMessage> {
  const toSign = producerMessageSigningBytes(msgType, payload, sender, epochMs);
  const signature = await sign(signingKeyRaw, toSign);
  return {
    type: msgType,
    payload,
    sender,
    epochMs,
    signature,
  };
}

/**
 * Verify a ProducerMessage's signature against the sender's public key.
 *
 * @returns true on success, false on any verification failure.
 */
export async function verifyProducerMessage(
  msg: ProducerMessage,
  peerPubkeyRaw: Uint8Array,
): Promise<boolean> {
  try {
    const toVerify = producerMessageSigningBytes(msg.type, msg.payload, msg.sender, msg.epochMs);
    return await verify(peerPubkeyRaw, toVerify, msg.signature);
  } catch {
    return false;
  }
}

// ── Gossip handler ──────────────────────────────────────────────────────────

/**
 * Process one inbound ProducerMessage.
 *
 * Normative processing order (S2.6, S2.10):
 * 1. Replay-window check.
 * 2. Sender membership check.
 * 3. Signature verification.
 * 4. Track clock offset / run drift detection.
 * 5. Dispatch by message type.
 *
 * @returns true if the message was accepted and dispatched, false otherwise.
 */
export async function handleProducerMessage(
  msg: ProducerMessage,
  opts: HandleMessageOptions,
): Promise<boolean> {
  const { state, config, peerPubkeys, registryCallback, driftTracker, onSelfDeparture, logger } = opts;
  const nowMs = Date.now();

  // 1. Replay-window check
  const delta = Math.abs(nowMs - msg.epochMs);
  if (delta > config.replayWindowMs) {
    logger?.debug(
      'gossip: dropping message from %s (outside replay window: delta=%dms)',
      msg.sender,
      delta,
    );
    return false;
  }

  // 2. Sender membership check
  if (!state.isPeerAccepted(msg.sender)) {
    logger?.warn(
      'gossip: SECURITY ALERT -- message from non-accepted sender %s; ' +
      'possible salt leak or deauthorized node still subscribed',
      msg.sender,
    );
    return false;
  }

  // 3. Signature verification
  const peerPubkey = peerPubkeys.get(msg.sender);
  if (peerPubkey == null) {
    logger?.warn(
      'gossip: SECURITY ALERT -- no public key for accepted sender %s; dropping message',
      msg.sender,
    );
    return false;
  }

  if (!(await verifyProducerMessage(msg, peerPubkey))) {
    logger?.warn(
      'gossip: SECURITY ALERT -- invalid signature from accepted sender %s',
      msg.sender,
    );
    return false;
  }

  // 4. Clock offset tracking + drift detection
  if (driftTracker != null) {
    const newlyIsolated = driftTracker.update(msg.sender, msg.epochMs);
    if (newlyIsolated) {
      logger?.warn(
        'gossip: peer %s clock drift exceeds tolerance; isolating',
        msg.sender,
      );
    } else if (!driftTracker.isIsolated(msg.sender)) {
      // Peer recovered -- already handled by driftTracker.update
    }
  }

  // 5. Message dispatch
  switch (msg.type) {
    case ProducerMessageType.INTRODUCE:
      _handleIntroduce(msg, state, logger);
      break;
    case ProducerMessageType.DEPART:
      _handleDepart(msg, state, driftTracker, onSelfDeparture, logger);
      break;
    case ProducerMessageType.CONTRACT_PUBLISHED:
      _handleContractPublished(msg, state, driftTracker, registryCallback, logger);
      break;
    case ProducerMessageType.LEASE_UPDATE:
      _handleLeaseUpdate(msg, state, driftTracker, registryCallback, logger);
      break;
    default:
      logger?.debug('gossip: unknown message type %d from %s; dropping', msg.type, msg.sender);
      return false;
  }

  return true;
}

// ── Type-specific dispatch helpers ──────────────────────────────────────────

function _handleIntroduce(
  msg: ProducerMessage,
  state: MeshState,
  logger?: HandleMessageOptions['logger'],
): void {
  const [ok, reason] = validateRcan(msg.payload);
  if (!ok) {
    logger?.debug('gossip: Introduce from %s has invalid rcan: %s', msg.sender, reason);
    return;
  }
  state.addPeer(msg.sender);
  logger?.info('gossip: Introduce accepted -- %s joined the mesh', msg.sender);
}

function _handleDepart(
  msg: ProducerMessage,
  state: MeshState,
  driftTracker?: ClockDriftTracker,
  onSelfDeparture?: () => void,
  logger?: HandleMessageOptions['logger'],
): void {
  state.remove(msg.sender);
  driftTracker?.removePeer(msg.sender);
  logger?.info('gossip: Depart -- %s left the mesh', msg.sender);
  void onSelfDeparture; // used by caller for self-departure detection
}

function _handleContractPublished(
  msg: ProducerMessage,
  _state: MeshState,
  driftTracker?: ClockDriftTracker,
  registryCallback?: HandleMessageOptions['registryCallback'],
  logger?: HandleMessageOptions['logger'],
): void {
  if (driftTracker?.isIsolated(msg.sender)) {
    logger?.debug('gossip: ContractPublished from drift-isolated peer %s; skipping', msg.sender);
    return;
  }
  if (registryCallback != null) {
    try {
      if (msg.payload.length > MAX_GOSSIP_PAYLOAD_SIZE) {
        logger?.warn('gossip: payload too large (%d bytes), dropping', msg.payload.length);
        return;
      }
      const d = JSON.parse(new TextDecoder().decode(msg.payload));
      const payload: ContractPublishedPayload = {
        serviceName: d.service_name,
        version: Number(d.version),
        contractCollectionHash: d.contract_collection_hash,
      };
      registryCallback('contract_published', payload);
    } catch (exc) {
      logger?.debug('gossip: malformed ContractPublished payload: %s', exc);
    }
  }
}

function _handleLeaseUpdate(
  msg: ProducerMessage,
  _state: MeshState,
  driftTracker?: ClockDriftTracker,
  registryCallback?: HandleMessageOptions['registryCallback'],
  logger?: HandleMessageOptions['logger'],
): void {
  if (driftTracker?.isIsolated(msg.sender)) {
    logger?.debug('gossip: LeaseUpdate from drift-isolated peer %s; skipping', msg.sender);
    return;
  }
  if (registryCallback != null) {
    try {
      if (msg.payload.length > MAX_GOSSIP_PAYLOAD_SIZE) {
        logger?.warn('gossip: LeaseUpdate payload too large (%d bytes)', msg.payload.length);
        return;
      }
      const d = JSON.parse(new TextDecoder().decode(msg.payload));
      const payload: LeaseUpdatePayload = {
        serviceName: d.service_name,
        version: Number(d.version),
        contractId: d.contract_id,
        healthStatus: d.health_status,
        addressingInfo: d.addressing_info ?? {},
      };
      registryCallback('lease_update', payload);
    } catch (exc) {
      logger?.debug('gossip: malformed LeaseUpdate payload: %s', exc);
    }
  }
}

// ── Payload serializers (JSON-based for Phase 12) ───────────────────────────

/** Encode IntroducePayload. The rcan grant is stored as raw bytes. */
export function encodeIntroducePayload(rcan: Uint8Array): Uint8Array {
  return rcan;
}

/** Encode DepartPayload as UTF-8 JSON. */
export function encodeDepartPayload(reason = ''): Uint8Array {
  return new TextEncoder().encode(JSON.stringify({ reason }));
}

/** Encode ContractPublishedPayload as UTF-8 JSON. */
export function encodeContractPublishedPayload(
  serviceName: string,
  version: number,
  contractCollectionHash: string,
): Uint8Array {
  return new TextEncoder().encode(
    JSON.stringify({
      service_name: serviceName,
      version,
      contract_collection_hash: contractCollectionHash,
    }),
  );
}

/** Encode LeaseUpdatePayload as UTF-8 JSON. */
export function encodeLeaseUpdatePayload(
  serviceName: string,
  version: number,
  contractId: string,
  healthStatus: string,
  addressingInfo: Record<string, string> = {},
): Uint8Array {
  return new TextEncoder().encode(
    JSON.stringify({
      service_name: serviceName,
      version,
      contract_id: contractId,
      health_status: healthStatus,
      addressing_info: addressingInfo,
    }),
  );
}

// ── Lease heartbeat ─────────────────────────────────────────────────────────

export interface StartLeaseHeartbeatOptions {
  /** A gossip topic handle with a broadcast(data: Uint8Array) method. */
  gossipTopicHandle: { broadcast(data: Uint8Array): Promise<void> };
  sender: string;
  signingKeyRaw: Uint8Array;
  serviceName: string;
  version: number;
  contractId: string;
  healthGetter: () => string;
  heartbeatIntervalMs?: number;
  addressingInfo?: Record<string, string>;
  logger?: HandleMessageOptions['logger'];
}

/**
 * Sign and broadcast a single LEASE_UPDATE message.
 */
export async function runLeaseHeartbeat(
  gossipTopicHandle: { broadcast(data: Uint8Array): Promise<void> },
  sender: string,
  signingKeyRaw: Uint8Array,
  serviceName: string,
  version: number,
  contractId: string,
  healthGetter: () => string,
  addressingInfo: Record<string, string> = {},
  logger?: HandleMessageOptions['logger'],
): Promise<void> {
  const epochMs = Date.now();
  const payload = encodeLeaseUpdatePayload(
    serviceName,
    version,
    contractId,
    healthGetter(),
    addressingInfo,
  );
  const msg = await signProducerMessage(
    ProducerMessageType.LEASE_UPDATE,
    payload,
    sender,
    epochMs,
    signingKeyRaw,
  );

  // Serialize the signed envelope as JSON for the gossip wire.
  const wire = new TextEncoder().encode(
    JSON.stringify({
      type: msg.type,
      payload: bytesToHex(msg.payload),
      sender: msg.sender,
      epoch_ms: msg.epochMs,
      signature: bytesToHex(msg.signature),
    }),
  );

  try {
    await gossipTopicHandle.broadcast(wire);
    logger?.debug(
      'heartbeat: LeaseUpdate broadcast for %s v%d (epoch=%d)',
      serviceName,
      version,
      epochMs,
    );
  } catch (exc) {
    logger?.warn('heartbeat: broadcast failed: %s', exc);
  }
}

/**
 * Spawn a periodic lease heartbeat that broadcasts LEASE_UPDATE messages.
 *
 * @returns An object with a stop() method to cancel the heartbeat.
 */
export function startLeaseHeartbeat(opts: StartLeaseHeartbeatOptions): { stop: () => void } {
  const {
    gossipTopicHandle,
    sender,
    signingKeyRaw,
    serviceName,
    version,
    contractId,
    healthGetter,
    heartbeatIntervalMs = 900_000,
    addressingInfo = {},
    logger,
  } = opts;

  const timer = setInterval(() => {
    runLeaseHeartbeat(
      gossipTopicHandle,
      sender,
      signingKeyRaw,
      serviceName,
      version,
      contractId,
      healthGetter,
      addressingInfo,
      logger,
    ).catch((err) => {
      logger?.warn('heartbeat: tick failed: %s', err);
    });
  }, heartbeatIntervalMs);

  return {
    stop() {
      clearInterval(timer);
    },
  };
}
