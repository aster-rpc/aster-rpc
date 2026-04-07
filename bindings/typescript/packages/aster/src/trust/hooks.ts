/**
 * Gate 0 connection hooks — ALPN-level connection gating.
 *
 * Spec reference: Aster-trust-spec.md
 *
 * The MeshEndpointHook runs a background loop reading hook events
 * from the IrohNode and accepting/denying connections based on
 * admission policy.
 */

/** Hook decision: allow or deny a connection. */
export interface HookDecision {
  allow: boolean;
  errorCode?: number;
  reason?: string;
}

/**
 * Connection gating policy.
 * Implement this to customize which connections are accepted.
 */
export interface ConnectionPolicy {
  /** Called before a connection is established. */
  onBeforeConnect(remoteEndpointId: string, alpn: Uint8Array): Promise<HookDecision>;
}

/** Default policy: allow all connections. */
export class AllowAllPolicy implements ConnectionPolicy {
  async onBeforeConnect(): Promise<HookDecision> {
    return { allow: true };
  }
}

/** Policy that denies all connections. */
export class DenyAllPolicy implements ConnectionPolicy {
  async onBeforeConnect(): Promise<HookDecision> {
    return { allow: false, errorCode: 1, reason: 'connections disabled' };
  }
}

// ── ALPN constants ────────────────────────────────────────────────────────────

const _encoder = new TextEncoder();
const _decoder = new TextDecoder();

/** Admission ALPN for producer enrollment. */
export const PRODUCER_ADMISSION_ALPN: Uint8Array = _encoder.encode('aster.producer_admission');

/** Admission ALPN for consumer enrollment. */
export const CONSUMER_ADMISSION_ALPN: Uint8Array = _encoder.encode('aster.consumer_admission');

const _ADMISSION_ALPN_STRINGS: ReadonlySet<string> = new Set([
  'aster.producer_admission',
  'aster.consumer_admission',
]);

// ── MeshEndpointHook ──────────────────────────────────────────────────────────

/**
 * Connection-level admission gate (Gate 0, S3.3).
 *
 * Maintains an allowlist of admitted peer endpoint IDs. The decision logic is:
 *
 * - Admission ALPNs (aster.producer_admission, aster.consumer_admission)
 *   -> always allow (credential presentation must be possible).
 * - Any other ALPN, peer in admitted set -> allow.
 * - Any other ALPN, peer NOT in admitted and allowUnenrolled=false -> deny.
 * - allowUnenrolled=true -> allow all (local/dev mode; must be explicit opt-in).
 */
export class MeshEndpointHook {
  readonly admitted: Set<string> = new Set();
  readonly allowUnenrolled: boolean;

  constructor(allowUnenrolled = false) {
    this.allowUnenrolled = allowUnenrolled;
  }

  // ── Decision logic ──────────────────────────────────────────────────────

  /**
   * Return true if this connection should be allowed.
   *
   * @param remoteEndpointId - NodeId of the connecting peer (from handshake).
   * @param alpn - ALPN negotiated for this connection (Uint8Array).
   */
  shouldAllow(remoteEndpointId: string, alpn: Uint8Array): boolean {
    // Admission ALPNs are always open — credential presentation
    const alpnStr = _decoder.decode(alpn);
    if (_ADMISSION_ALPN_STRINGS.has(alpnStr)) {
      return true;
    }
    // Admitted peers are always allowed
    if (this.admitted.has(remoteEndpointId)) {
      return true;
    }
    // Open-mode bypass (local/dev only)
    if (this.allowUnenrolled) {
      return true;
    }
    return false;
  }

  // ── Allowlist management ────────────────────────────────────────────────

  /** Add a peer to the admitted set after successful credential check. */
  addPeer(endpointId: string): void {
    this.admitted.add(endpointId);
  }

  /** Remove a peer from the admitted set (e.g., on lease expiry). */
  removePeer(endpointId: string): void {
    this.admitted.delete(endpointId);
  }

  // ── Iroh hook-loop integration ──────────────────────────────────────────

  /**
   * Background loop: poll hookReceiver and apply Gate 0 decisions.
   *
   * Wires this hook to Iroh's Phase 1b HookReceiver. Run via a detached
   * promise after obtaining the receiver from netClient.takeHookReceiver().
   *
   * @param hookReceiver - An object with recvBeforeConnect() that returns
   *   { info: { remoteEndpointId: string; alpn: Uint8Array }, respond(d: HookDecision): Promise<void> } | null
   */
  async runHookLoop(hookReceiver: {
    recvBeforeConnect(): Promise<{
      info: { remoteEndpointId: string; alpn: Uint8Array };
      respond(decision: HookDecision): Promise<void>;
    } | null>;
  }): Promise<void> {
    try {
      while (true) {
        const event = await hookReceiver.recvBeforeConnect();
        if (event == null) break;

        const { info, respond } = event;
        if (this.shouldAllow(info.remoteEndpointId, info.alpn)) {
          await respond({ allow: true });
        } else {
          await respond({ allow: false, errorCode: 403, reason: 'not admitted' });
        }
      }
    } catch (err: unknown) {
      // Cancellation or shutdown — silently exit
      const msg = err instanceof Error ? err.message : String(err);
      if (msg.includes('abort') || msg.includes('cancel')) return;
      // Log unexpected errors (no logger abstraction yet — console.error)
      console.error('Hook loop error:', msg);
    }
  }
}
