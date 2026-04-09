/**
 * aster/peer-store -- Per-peer admission attribute store.
 *
 * Bridges the gap between admission (where attributes are determined)
 * and RPC dispatch (where attributes are needed for authorization).
 *
 * Both consumer admission and delegated admission handlers write to this
 * store on successful admission. The RPC server reads from it when
 * building CallContext for each call.
 */

/** Record of a successful peer admission. */
export interface PeerAdmission {
  endpointId: string;
  handle: string;
  attributes: Map<string, string>;
  admittedAt: number;
  /** "consumer_admission" | "aster.admission" */
  admissionPath: string;
}

/** Create a PeerAdmission with defaults for optional fields. */
export function createPeerAdmission(
  opts: Pick<PeerAdmission, "endpointId"> & Partial<Omit<PeerAdmission, "endpointId">>,
): PeerAdmission {
  return {
    endpointId: opts.endpointId,
    handle: opts.handle ?? "",
    attributes: opts.attributes ?? new Map(),
    admittedAt: opts.admittedAt ?? Date.now() / 1000,
    admissionPath: opts.admissionPath ?? "",
  };
}

/**
 * In-memory store mapping peer endpointId to admission attributes.
 *
 * Safe for concurrent reads/writes from admission handlers
 * and the RPC server (single-threaded JS, so no lock needed).
 */
export class PeerAttributeStore {
  private peers: Map<string, PeerAdmission> = new Map();

  /** Record a successful admission. */
  admit(admission: PeerAdmission): void {
    this.peers.set(admission.endpointId, admission);
  }

  /** Look up admission record for a peer. */
  get(endpointId: string): PeerAdmission | undefined {
    return this.peers.get(endpointId);
  }

  /** Remove a peer on disconnect or revocation. */
  remove(endpointId: string): void {
    this.peers.delete(endpointId);
  }

  /** Get attributes map for a peer, or empty map if not admitted. */
  getAttributes(endpointId: string): Map<string, string> {
    const admission = this.peers.get(endpointId);
    return admission ? new Map(admission.attributes) : new Map();
  }
}
