/**
 * aster/peer-store -- the admission-record view the frozen credential
 * handlers (`trust/consumer.ts`, `trust/delegated.ts`) and RPC dispatch use,
 * backed by the Rust Gate-0 {@link PeerAdmissionStore}.
 *
 * There is one store: admitting a peer here admits it into the gate's store,
 * so it passes Gate 0 on the RPC ALPN; its attributes feed RPC
 * authorization; expiry is enforced by Rust.
 *
 * Config: ASTER_PEER_TTL_S env var (default 86400 = 24h) caps how long a
 * peer stays admitted.
 */

import { PeerAdmissionStore } from './gate.js';

/** Server-side upper bound on peer admission lifetime (seconds). */
export const PEER_TTL_S: number = (() => {
  if (typeof process !== 'undefined' && process.env?.ASTER_PEER_TTL_S) {
    return parseFloat(process.env.ASTER_PEER_TTL_S);
  }
  return 86400;
})();

const _REAPER_INTERVAL_MS = 300_000;

/** Record of a successful peer admission (credential-handler shape). */
export interface PeerAdmission {
  endpointId: string;
  handle: string;
  attributes: Map<string, string>;
  /** Epoch seconds. */
  admittedAt: number;
  /** Epoch seconds; 0 = no credential expiry (PEER_TTL_S still applies). */
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

/**
 * Admission records over the Rust {@link PeerAdmissionStore}.
 */
export class PeerAttributeStore {
  readonly store: PeerAdmissionStore;
  private reaperTimer: ReturnType<typeof setInterval> | null = null;

  constructor(store: PeerAdmissionStore = new PeerAdmissionStore()) {
    this.store = store;
  }

  /** Record a successful admission (admits the peer through Gate 0). */
  admit(admission: PeerAdmission): void {
    const ttlEnd = (admission.admittedAt + PEER_TTL_S) * 1000;
    const credEnd = admission.expiresAt > 0 ? admission.expiresAt * 1000 : Infinity;
    this.store.admit(admission.endpointId, {
      attributes: admission.attributes,
      expiresAt: Math.max(0, Math.floor(Math.min(ttlEnd, credEnd))),
      source: admission.admissionPath === 'aster.admission' ? 'attestation' : 'credential',
    });
  }

  /** The unexpired admission of a peer, if any. */
  get(endpointId: string): PeerAdmission | undefined {
    const a = this.store.get(endpointId);
    if (!a) return undefined;
    return {
      endpointId: a.peer,
      handle: '',
      attributes: new Map(Object.entries(a.attributes)),
      admittedAt: a.admittedAt.getTime() / 1000,
      expiresAt: a.expiresAt ? a.expiresAt.getTime() / 1000 : 0,
      admissionPath: a.source === 'attestation' ? 'aster.admission' : 'consumer_admission',
    };
  }

  /** Remove a peer (revocation). */
  remove(endpointId: string): void {
    this.store.revoke(endpointId);
  }

  /** Attributes of an unexpired admission, or an empty map. */
  getAttributes(endpointId: string): Map<string, string> {
    try {
      return new Map(Object.entries(this.store.attributes(endpointId)));
    } catch {
      return new Map(); // not a node id: never admitted
    }
  }

  /** Remove all expired entries. Returns count removed. */
  sweepExpired(): number {
    return this.store.pruneExpired();
  }

  get peerCount(): number {
    return this.store.size;
  }

  /** Start background pruning (call once at server start). */
  startReaper(): void {
    if (this.reaperTimer != null) return;
    this.reaperTimer = setInterval(() => this.sweepExpired(), _REAPER_INTERVAL_MS);
    this.reaperTimer.unref?.();
  }

  /** Stop background pruning. */
  stopReaper(): void {
    if (this.reaperTimer != null) {
      clearInterval(this.reaperTimer);
      this.reaperTimer = null;
    }
  }
}
