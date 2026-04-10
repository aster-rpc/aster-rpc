/**
 * aster/peer-store -- Per-peer admission attribute store.
 *
 * Bridges the gap between admission (where attributes are determined)
 * and RPC dispatch (where attributes are needed for authorization).
 *
 * Expiry: each admission records the credential's `expiresAt` (epoch
 * seconds). Lookups check expiry and lazily evict stale entries. A
 * background reaper sweeps entries that are never looked up again.
 *
 * Config: ASTER_PEER_TTL_S env var (default 86400 = 24h) sets the
 * server-side upper bound on how long a peer stays admitted.
 */

/** Server-side upper bound on peer admission lifetime (seconds). */
export const PEER_TTL_S: number = (() => {
  if (typeof process !== 'undefined' && process.env?.ASTER_PEER_TTL_S) {
    return parseFloat(process.env.ASTER_PEER_TTL_S);
  }
  return 86400;
})();

const _REAPER_INTERVAL_MS = 300_000;

/** Record of a successful peer admission. */
export interface PeerAdmission {
  endpointId: string;
  handle: string;
  attributes: Map<string, string>;
  admittedAt: number;
  expiresAt: number;
  /** "consumer_admission" | "aster.admission" */
  admissionPath: string;
}

/** Create a PeerAdmission with defaults for optional fields. */
export function createPeerAdmission(
  opts: Pick<PeerAdmission, 'endpointId'> & Partial<Omit<PeerAdmission, 'endpointId'>>,
): PeerAdmission {
  return {
    endpointId: opts.endpointId,
    handle: opts.handle ?? '',
    attributes: opts.attributes ?? new Map(),
    admittedAt: opts.admittedAt ?? Date.now() / 1000,
    expiresAt: opts.expiresAt ?? 0,
    admissionPath: opts.admissionPath ?? '',
  };
}

function isExpired(admission: PeerAdmission): boolean {
  const now = Date.now() / 1000;
  if (admission.expiresAt > 0 && now > admission.expiresAt) return true;
  if (now > admission.admittedAt + PEER_TTL_S) return true;
  return false;
}

/**
 * In-memory store mapping peer endpointId to admission attributes.
 *
 * Entries are lazily evicted on access when expired. Call startReaper()
 * to enable periodic sweeps of entries that are never accessed.
 */
export class PeerAttributeStore {
  private peers: Map<string, PeerAdmission> = new Map();
  private reaperTimer: ReturnType<typeof setInterval> | null = null;

  /** Record a successful admission. */
  admit(admission: PeerAdmission): void {
    this.peers.set(admission.endpointId, admission);
  }

  /** Look up admission record for a peer. Returns undefined if expired. */
  get(endpointId: string): PeerAdmission | undefined {
    const admission = this.peers.get(endpointId);
    if (!admission) return undefined;
    if (isExpired(admission)) {
      this.peers.delete(endpointId);
      return undefined;
    }
    return admission;
  }

  /** Remove a peer on disconnect or revocation. */
  remove(endpointId: string): void {
    this.peers.delete(endpointId);
  }

  /** Get attributes map for a peer, or empty map if expired/not admitted. */
  getAttributes(endpointId: string): Map<string, string> {
    const admission = this.get(endpointId);
    return admission ? new Map(admission.attributes) : new Map();
  }

  /** Remove all expired entries. Returns count removed. */
  sweepExpired(): number {
    let count = 0;
    for (const [eid, adm] of this.peers) {
      if (isExpired(adm)) {
        this.peers.delete(eid);
        count++;
      }
    }
    return count;
  }

  get peerCount(): number {
    return this.peers.size;
  }

  /** Start background reaper (call once at server start). */
  startReaper(): void {
    if (this.reaperTimer != null) return;
    this.reaperTimer = setInterval(() => this.sweepExpired(), _REAPER_INTERVAL_MS);
    if (typeof this.reaperTimer === 'object' && 'unref' in this.reaperTimer) {
      (this.reaperTimer as any).unref();
    }
  }

  /** Stop background reaper. */
  stopReaper(): void {
    if (this.reaperTimer != null) {
      clearInterval(this.reaperTimer);
      this.reaperTimer = null;
    }
  }
}
