/**
 * Nonce store for OTT (one-time-token) replay protection.
 *
 * Spec reference: Aster-trust-spec.md
 *
 * Tracks consumed nonces to prevent credential replay attacks.
 * Each OTT nonce can only be used once.
 */

/** Nonce store interface. */
export interface NonceStore {
  /** Check if a nonce has been consumed. */
  has(nonce: string): boolean;
  /** Mark a nonce as consumed. */
  consume(nonce: string): void;
  /** Number of consumed nonces. */
  readonly size: number;
}

/**
 * In-memory nonce store with optional TTL expiry.
 *
 * Nonces are stored in a Map with their consumption timestamp.
 * Expired nonces are cleaned up periodically to prevent unbounded growth.
 */
export class InMemoryNonceStore implements NonceStore {
  private nonces = new Map<string, number>(); // nonce -> consumed_epoch_ms
  private ttlMs: number;

  /**
   * @param ttlMs - How long to remember consumed nonces (default: 24 hours).
   *   After this time, a nonce could theoretically be replayed, but the
   *   credential itself should have expired by then.
   */
  constructor(ttlMs = 24 * 60 * 60 * 1000) {
    this.ttlMs = ttlMs;
  }

  has(nonce: string): boolean {
    this.cleanup();
    return this.nonces.has(nonce);
  }

  consume(nonce: string): void {
    this.nonces.set(nonce, Date.now());
  }

  get size(): number {
    this.cleanup();
    return this.nonces.size;
  }

  /** Remove expired nonces. */
  private cleanup(): void {
    const cutoff = Date.now() - this.ttlMs;
    for (const [nonce, ts] of this.nonces) {
      if (ts < cutoff) this.nonces.delete(nonce);
    }
  }
}
