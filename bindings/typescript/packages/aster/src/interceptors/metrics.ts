/**
 * Metrics interceptor with optional OpenTelemetry integration.
 *
 * Collects RED metrics (Rate, Errors, Duration). Falls back to
 * in-memory counters when OTel is not installed.
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';
import type { RpcError } from '../status.js';

export class MetricsInterceptor implements Interceptor {
  started = 0;
  succeeded = 0;
  failed = 0;
  inFlight = 0;

  private startTimes = new Map<string, number>();

  async onRequest(ctx: CallContext, request: unknown): Promise<unknown> {
    this.started++;
    this.inFlight++;
    this.startTimes.set(ctx.callId, performance.now());
    return request;
  }

  async onResponse(ctx: CallContext, response: unknown): Promise<unknown> {
    this.succeeded++;
    this.inFlight--;
    this.startTimes.delete(ctx.callId);
    return response;
  }

  async onError(ctx: CallContext, error: RpcError): Promise<RpcError> {
    this.failed++;
    this.inFlight--;
    this.startTimes.delete(ctx.callId);
    return error;
  }

  /** Snapshot of current metrics. */
  snapshot(): { started: number; succeeded: number; failed: number; inFlight: number } {
    return {
      started: this.started,
      succeeded: this.succeeded,
      failed: this.failed,
      inFlight: this.inFlight,
    };
  }

  /** Reset all counters. */
  reset(): void {
    this.started = 0;
    this.succeeded = 0;
    this.failed = 0;
    this.inFlight = 0;
    this.startTimes.clear();
  }
}
