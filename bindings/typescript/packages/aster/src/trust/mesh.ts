/**
 * Mesh state — gossip-based service discovery.
 *
 * Tracks which peers offer which services, updated via gossip broadcasts.
 */

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

  /** Record services from a peer. */
  update(peerEndpointId: string, services: PeerService[]): void {
    this.peers.set(peerEndpointId, services);
  }

  /** Remove a peer. */
  remove(peerEndpointId: string): void {
    this.peers.delete(peerEndpointId);
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
}
