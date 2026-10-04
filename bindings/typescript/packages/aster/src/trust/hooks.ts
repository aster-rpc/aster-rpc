/**
 * Admission ALPNs and the admit hook the frozen credential handlers call.
 *
 * Gate 0 itself (which peer may use which ALPN) is decided in Rust — see
 * `GatePolicy` in `../gate.ts`. This module only names the ALPNs that stay
 * open to un-admitted peers and the narrow interface the credential
 * handlers use to admit a peer after a successful credential check.
 */

import type { GatePolicy } from '../gate.js';

const _encoder = new TextEncoder();

/** Admission ALPN for producer enrollment. */
export const PRODUCER_ADMISSION_ALPN: Uint8Array = _encoder.encode('aster.producer_admission');

/** Admission ALPN for consumer enrollment. */
export const CONSUMER_ADMISSION_ALPN: Uint8Array = _encoder.encode('aster.consumer_admission');

/** Admission ALPN for delegated enrollment. */
export const DELEGATED_ADMISSION_ALPN: Uint8Array = _encoder.encode('aster.admission');

/**
 * What a credential handler calls after admitting a peer. Implemented over a
 * Rust `GatePolicy` by {@link gateAdmitter}.
 */
export interface PeerAdmitter {
  /** Admit `endpointId` through Gate 0 (no-op if already admitted). */
  addPeer(endpointId: string): void;
  /** Revoke `endpointId`. */
  removePeer(endpointId: string): void;
}

/**
 * A {@link PeerAdmitter} over `gate`. `addPeer` never replaces an existing
 * record, so attributes / expiry a handler stored first are kept.
 */
export function gateAdmitter(gate: GatePolicy): PeerAdmitter {
  return {
    addPeer(endpointId: string): void {
      if (!gate.isAdmitted(endpointId)) gate.admit(endpointId, { source: 'credential' });
    },
    removePeer(endpointId: string): void {
      gate.revoke(endpointId);
    },
  };
}
