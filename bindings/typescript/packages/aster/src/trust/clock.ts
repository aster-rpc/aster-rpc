/**
 * Clock drift detection for mesh peers.
 *
 * Spec reference: Aster-trust-spec.md
 *
 * Detects excessive clock skew between mesh peers by comparing
 * timestamps in gossip heartbeats. Peers with drift beyond
 * the configured tolerance are isolated.
 */

/** Clock drift configuration. */
export interface ClockDriftConfig {
  /** Maximum allowed drift in milliseconds (default: 30 seconds). */
  toleranceMs: number;
  /** Grace period after mesh join before drift checks kick in (default: 60 seconds). */
  gracePeriodMs: number;
}

/** Default clock drift config. */
export const DEFAULT_CLOCK_DRIFT_CONFIG: ClockDriftConfig = {
  toleranceMs: 30_000,
  gracePeriodMs: 60_000,
};

/**
 * Check if a peer's timestamp indicates excessive clock drift.
 *
 * @param peerTimestampMs - Epoch ms from the peer's heartbeat
 * @param localTimestampMs - Local epoch ms (default: Date.now())
 * @param config - Drift configuration
 * @returns The drift in milliseconds (positive = peer is ahead, negative = behind)
 */
export function computeDrift(
  peerTimestampMs: number,
  localTimestampMs = Date.now(),
): number {
  return peerTimestampMs - localTimestampMs;
}

/**
 * Check whether a peer should be isolated due to clock drift.
 *
 * @param driftMs - The computed drift (from computeDrift)
 * @param meshJoinedAtMs - When we joined the mesh
 * @param config - Drift configuration
 * @returns true if the peer should be isolated
 */
export function shouldIsolate(
  driftMs: number,
  meshJoinedAtMs: number,
  config: ClockDriftConfig = DEFAULT_CLOCK_DRIFT_CONFIG,
): boolean {
  // Don't isolate during grace period
  const elapsed = Date.now() - meshJoinedAtMs;
  if (elapsed < config.gracePeriodMs) return false;

  return Math.abs(driftMs) > config.toleranceMs;
}

/**
 * Clock drift tracker for multiple peers.
 *
 * Records per-peer clock offsets and tracks which peers have been
 * isolated due to excessive drift.
 */
export class ClockDriftTracker {
  private offsets = new Map<string, number>(); // peer -> latest drift ms
  private isolated = new Set<string>();
  private meshJoinedAtMs: number;
  private config: ClockDriftConfig;

  constructor(meshJoinedAtMs = Date.now(), config?: Partial<ClockDriftConfig>) {
    this.meshJoinedAtMs = meshJoinedAtMs;
    this.config = { ...DEFAULT_CLOCK_DRIFT_CONFIG, ...config };
  }

  /**
   * Update a peer's clock offset from a heartbeat.
   *
   * @returns true if the peer was newly isolated
   */
  update(peerEndpointId: string, peerTimestampMs: number): boolean {
    const drift = computeDrift(peerTimestampMs);
    this.offsets.set(peerEndpointId, drift);

    if (shouldIsolate(drift, this.meshJoinedAtMs, this.config)) {
      if (!this.isolated.has(peerEndpointId)) {
        this.isolated.add(peerEndpointId);
        return true;
      }
    } else {
      // Peer recovered — remove from isolation
      this.isolated.delete(peerEndpointId);
    }

    return false;
  }

  /** Check if a peer is currently isolated. */
  isIsolated(peerEndpointId: string): boolean {
    return this.isolated.has(peerEndpointId);
  }

  /** Get drift for a peer in ms. */
  getDrift(peerEndpointId: string): number | undefined {
    return this.offsets.get(peerEndpointId);
  }

  /** All isolated peers. */
  isolatedPeers(): string[] {
    return [...this.isolated];
  }
}
