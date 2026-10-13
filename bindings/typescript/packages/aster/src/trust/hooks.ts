/**
 * The delegated-admission ALPN and the admit hook its handler calls.
 *
 * Gate 0 itself (which peer may use which ALPN) is decided in Rust — see
 * `GatePolicy` in `../gate.ts`. This module only names the delegated
 * admission ALPN and the narrow interface `handleDelegatedAdmissionConnection`
 * uses to admit a peer after a successful check. (The consumer and producer
 * admission ALPNs are retired: `AsterServer` admits peers with Enrollment.)
 */

import type { GatePolicy } from '../gate.js';

const _encoder = new TextEncoder();

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
