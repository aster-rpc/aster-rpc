/**
 * Retry interceptor with exponential backoff.
 *
 * Only retries idempotent calls on retryable status codes.
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';
import { RpcError, StatusCode } from '../status.js';
import { DEFAULT_RETRY, type RetryPolicy } from '../types.js';

const RETRYABLE_CODES = new Set<number>([
  StatusCode.UNAVAILABLE,
  StatusCode.DEADLINE_EXCEEDED,
  StatusCode.ABORTED,
  StatusCode.RESOURCE_EXHAUSTED,
]);

export class RetryInterceptor implements Interceptor {
  private policy: RetryPolicy;

  constructor(policy?: Partial<RetryPolicy>) {
    this.policy = { ...DEFAULT_RETRY, ...policy };
  }

  async onError(ctx: CallContext, error: RpcError): Promise<RpcError> {
    if (!ctx.idempotent) return error;
    if (!RETRYABLE_CODES.has(error.code)) return error;
    if (ctx.attempt >= this.policy.maxAttempts) return error;

    // Signal retry by attaching metadata
    error.details['retry_after_ms'] = String(this.backoffMs(ctx.attempt));
    error.details['retry_attempt'] = String(ctx.attempt);
    return error;
  }

  private backoffMs(attempt: number): number {
    const { initialMs, multiplier, maxMs, jitter } = this.policy.backoff;
    const base = Math.min(initialMs * Math.pow(multiplier, attempt - 1), maxMs);
    const jitterAmount = base * jitter * (Math.random() * 2 - 1);
    return Math.max(0, Math.round(base + jitterAmount));
  }

  /**
   * Decide if a call should be retried.
   * Returns true if the error is retryable, call is idempotent, and attempts remain.
   */
  shouldRetry(error: RpcError, attempt: number, idempotent = false): boolean {
    if (!idempotent) return false;
    if (!RETRYABLE_CODES.has(error.code)) return false;
    return attempt < this.policy.maxAttempts;
  }

  /**
   * Compute backoff duration in seconds for the given attempt number.
   */
  backoffSeconds(attempt: number): number {
    return this.backoffMs(attempt) / 1000;
  }
}
