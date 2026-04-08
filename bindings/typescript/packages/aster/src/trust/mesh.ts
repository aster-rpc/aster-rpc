/**
 * Mesh state — gossip-based service discovery.
 *
 * Tracks which peers offer which services, updated via gossip broadcasts.
 * Supports JSON persistence to ~/.aster/mesh_state.json.
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync, renameSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { homedir } from 'node:os';

/** A service available from a peer. */
export interface PeerService {
  peerEndpointId: string;
  serviceName: string;
  serviceVersion: number;
  contractId: string;
}

/** Mesh state: tracks discovered peers and their services. */
export class MeshState {
  private peers = new Map<string, PeerService[]>();
  private _acceptedProducers = new Set<string>();
  /** Hex-encoded 32-byte gossip topic ID for the producer mesh. */
  topicId = '';

  /** Add a peer to the accepted set (used by producer admission). */
  addPeer(peerEndpointId: string): void {
    this._acceptedProducers.add(peerEndpointId);
    if (!this.peers.has(peerEndpointId)) {
      this.peers.set(peerEndpointId, []);
    }
  }

  /** Check if a peer is in the accepted set. */
  isPeerAccepted(peerEndpointId: string): boolean {
    return this._acceptedProducers.has(peerEndpointId);
  }

  /** Record services from a peer. */
  update(peerEndpointId: string, services: PeerService[]): void {
    this.peers.set(peerEndpointId, services);
  }

  /** Remove a peer. */
  remove(peerEndpointId: string): void {
    this.peers.delete(peerEndpointId);
    this._acceptedProducers.delete(peerEndpointId);
  }

  /** Find peers offering a service by name. */
  findService(serviceName: string): PeerService[] {
    const results: PeerService[] = [];
    for (const services of this.peers.values()) {
      for (const svc of services) {
        if (svc.serviceName === serviceName) results.push(svc);
      }
    }
    return results;
  }

  /** All known peers. */
  allPeers(): string[] {
    return [...this.peers.keys()];
  }

  /** Number of known peers. */
  get size(): number {
    return this.peers.size;
  }

  /** Serialize to JSON-compatible dict. */
  toJson(): Record<string, unknown> {
    return {
      accepted_producers: [...this._acceptedProducers],
      peers: Object.fromEntries(
        [...this.peers.entries()].map(([k, v]) => [k, v]),
      ),
      topic_id: this.topicId,
    };
  }

  /** Deserialize from JSON dict. */
  static fromJson(data: Record<string, unknown>): MeshState {
    const state = new MeshState();
    const producers = data.accepted_producers as string[] | undefined;
    if (producers) {
      for (const p of producers) state._acceptedProducers.add(p);
    }
    const peers = data.peers as Record<string, PeerService[]> | undefined;
    if (peers) {
      for (const [k, v] of Object.entries(peers)) {
        state.peers.set(k, v);
      }
    }
    state.topicId = (data.topic_id as string) ?? '';
    return state;
  }
}

// -- Mesh state persistence ---------------------------------------------------

const MESH_STATE_DIR = join(homedir(), '.aster');
const MESH_STATE_FILE = join(MESH_STATE_DIR, 'mesh_state.json');

/** Save mesh state to ~/.aster/mesh_state.json (atomic rename). */
export function saveMeshState(state: MeshState, path = MESH_STATE_FILE): void {
  const dir = dirname(path);
  if (!existsSync(dir)) mkdirSync(dir, { recursive: true });
  const tmp = path + '.tmp';
  writeFileSync(tmp, JSON.stringify(state.toJson(), null, 2));
  renameSync(tmp, path);
}

/** Load mesh state from ~/.aster/mesh_state.json. Returns null if not found. */
export function loadMeshState(path = MESH_STATE_FILE): MeshState | null {
  try {
    if (!existsSync(path)) return null;
    const data = JSON.parse(readFileSync(path, 'utf-8'));
    return MeshState.fromJson(data);
  } catch {
    return null;
  }
}
