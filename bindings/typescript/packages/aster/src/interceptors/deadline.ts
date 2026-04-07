/**
 * Deadline enforcement interceptor. Spec S6.8.1.
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';
import { RpcError, StatusCode } from '../status.js';

export class DeadlineInterceptor implements Interceptor {
  private skewToleranceMs: number;

  constructor(skewToleranceMs = 5000) {
    this.skewToleranceMs = skewToleranceMs;
  }

  async onRequest(ctx: CallContext, request: unknown): Promise<unknown> {
    if (ctx.deadline !== undefined) {
      const nowMs = Date.now();
      const deadlineMs = ctx.deadline * 1000;
      if (nowMs > deadlineMs + this.skewToleranceMs) {
        throw new RpcError(
          StatusCode.DEADLINE_EXCEEDED,
          `deadline already expired on receipt (skew_tolerance=${this.skewToleranceMs}ms)`,
        );
      }
      if (ctx.expired) {
        throw new RpcError(StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
      }
    }
    return request;
  }

  timeoutSeconds(ctx: CallContext): number | undefined {
    const remaining = ctx.remainingSeconds;
    return remaining !== undefined ? Math.max(0, remaining) : undefined;
  }
}
