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

  /** Total duration of all completed RPCs in seconds (for computing averages). */
  totalDurationS = 0;
  /** Duration of the last completed RPC in seconds. */
  lastDurationS = 0;

  private startTimes = new Map<string, number>();

  // Optional OTel integration
  private _tracer: unknown = null;
  private _meter: unknown = null;
  private _startedCounter: unknown = null;
  private _completedCounter: unknown = null;
  private _durationHistogram: unknown = null;

  constructor() {
    try {
      // eslint-disable-next-line @typescript-eslint/no-require-imports
      const { trace, metrics } = require('@opentelemetry/api');
      this._tracer = trace.getTracer('aster.rpc', '0.1.0');
      this._meter = metrics.getMeter('aster.rpc', '0.1.0');
      this._startedCounter = (this._meter as any).createCounter('aster.rpc.started', {
        description: 'Total RPC calls started',
        unit: '1',
      });
      this._completedCounter = (this._meter as any).createCounter('aster.rpc.completed', {
        description: 'Total RPC calls completed',
        unit: '1',
      });
      this._durationHistogram = (this._meter as any).createHistogram('aster.rpc.duration', {
        description: 'RPC call duration',
        unit: 's',
      });
    } catch {
      // OTel not installed — use fallback counters
    }
  }

  /** Whether OpenTelemetry is available and configured. */
  get hasOtel(): boolean {
    return this._tracer !== null;
  }

  async onRequest(ctx: CallContext, request: unknown): Promise<unknown> {
    this.started++;
    this.inFlight++;
    this.startTimes.set(ctx.callId, performance.now());

    if (this._startedCounter) {
      const labels = { service: ctx.service, method: ctx.method, pattern: ctx.pattern ?? 'unary' };
      (this._startedCounter as any).add(1, labels);
    }

    return request;
  }

  async onResponse(ctx: CallContext, response: unknown): Promise<unknown> {
    this.succeeded++;
    this.inFlight--;

    const startTime = this.startTimes.get(ctx.callId);
    if (startTime !== undefined) {
      const durationS = (performance.now() - startTime) / 1000;
      this.totalDurationS += durationS;
      this.lastDurationS = durationS;
      this.startTimes.delete(ctx.callId);

      if (this._durationHistogram) {
        (this._durationHistogram as any).record(durationS, {
          service: ctx.service,
          method: ctx.method,
        });
      }
    }

    if (this._completedCounter) {
      (this._completedCounter as any).add(1, {
        service: ctx.service,
        method: ctx.method,
        status: 'OK',
      });
    }

    return response;
  }

  async onError(ctx: CallContext, error: RpcError): Promise<RpcError> {
    this.failed++;
    this.inFlight--;

    const startTime = this.startTimes.get(ctx.callId);
    if (startTime !== undefined) {
      const durationS = (performance.now() - startTime) / 1000;
      this.totalDurationS += durationS;
      this.lastDurationS = durationS;
      this.startTimes.delete(ctx.callId);

      if (this._durationHistogram) {
        (this._durationHistogram as any).record(durationS, {
          service: ctx.service,
          method: ctx.method,
        });
      }
    }

    if (this._completedCounter) {
      (this._completedCounter as any).add(1, {
        service: ctx.service,
        method: ctx.method,
        status: String((error as any).code ?? 'UNKNOWN'),
      });
    }

    return error;
  }

  /** Snapshot of current metrics. */
  snapshot(): {
    started: number;
    succeeded: number;
    failed: number;
    inFlight: number;
    totalDurationS: number;
    lastDurationS: number;
  } {
    return {
      started: this.started,
      succeeded: this.succeeded,
      failed: this.failed,
      inFlight: this.inFlight,
      totalDurationS: this.totalDurationS,
      lastDurationS: this.lastDurationS,
    };
  }

  /** Reset all counters. */
  reset(): void {
    this.started = 0;
    this.succeeded = 0;
    this.failed = 0;
    this.inFlight = 0;
    this.totalDurationS = 0;
    this.lastDurationS = 0;
    this.startTimes.clear();
  }
}
