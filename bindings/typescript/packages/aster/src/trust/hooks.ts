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
