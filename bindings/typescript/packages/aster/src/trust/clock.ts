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
  /** Minimum number of peers required for mesh median computation (default: 3). */
  minPeers: number;
}

/** Default clock drift config. */
export const DEFAULT_CLOCK_DRIFT_CONFIG: ClockDriftConfig = {
  toleranceMs: 30_000,
  gracePeriodMs: 60_000,
  minPeers: 3,
};

/**
 * Check if a peer's timestamp indicates excessive clock drift.
 *
 * @param peerTimestampMs - Epoch ms from the peer's heartbeat
 * @param localTimestampMs - Local epoch ms (default: Date.now())
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
 * Compute the high median of an array of numbers.
 *
 * For odd-length arrays, returns the middle element.
 * For even-length arrays, returns the higher of the two middle elements
 * (matches Python's statistics.median_high for determinism).
 *
 * @param values - Array of numbers (must not be empty).
 */
function medianHigh(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted[mid];
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

  /** Return a copy of the current peer offset map. */
  peerOffsets(): Map<string, number> {
    return new Map(this.offsets);
  }

  /**
   * Compute the mesh median offset from all tracked peers.
   *
   * Returns undefined if fewer than minPeers peers are tracked
   * (not enough data for meaningful drift detection).
   *
   * Uses median_high (higher-middle for even counts) for determinism.
   */
  meshMedianOffset(): number | undefined {
    const values = [...this.offsets.values()];
    if (values.length < this.config.minPeers) {
      return undefined;
    }
    return medianHigh(values);
  }

  /**
   * Check if this node's own clock appears to be the outlier.
   *
   * @param selfOffsetEstimate - now_ms - msg.epoch_ms computed when this node
   *   sends a message (i.e. ~0 if the clock is correct).
   * @returns true if self is the outlier. Returns false during grace period or
   *   when there are too few peers.
   */
  selfInDrift(selfOffsetEstimate: number): boolean {
    const elapsed = Date.now() - this.meshJoinedAtMs;
    if (elapsed < this.config.gracePeriodMs) return false;

    const median = this.meshMedianOffset();
    if (median === undefined) return false;

    return Math.abs(selfOffsetEstimate - median) > this.config.toleranceMs;
  }

  /**
   * Remove a peer from the offset tracking table and isolation set.
   * Called on Depart or lease expiry.
   */
  removePeer(endpointId: string): void {
    this.offsets.delete(endpointId);
    this.isolated.delete(endpointId);
  }
}
