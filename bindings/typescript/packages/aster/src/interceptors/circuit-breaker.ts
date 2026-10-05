/**
 * Circuit breaker interceptor — stops sending requests to failing services.
 *
 * States: CLOSED (normal) -> OPEN (failing) -> HALF_OPEN (probe) -> CLOSED
 *
 * Semantics follow the Python client (`aster/interceptors/circuit_breaker.py`):
 * only UNAVAILABLE / INTERNAL / UNKNOWN count as failures; a failed half-open
 * probe reopens the circuit; any other outcome that says nothing about the
 * peer's health (another error code, a cancelled call, a refusal by another
 * breaker) gives the probe slot back via {@link releaseProbe}.
 *
 * On a client (`createClient(..., { interceptors })`) the call loop drives
 * {@link beforeCall} / {@link recordSuccess} / {@link recordFailure} directly
 * and does not run this as a hook. The hook methods remain for server chains.
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';
import { RpcError, StatusCode } from '../status.js';

type State = 'closed' | 'open' | 'half_open';

const COUNTED_CODES = new Set<number>([
  StatusCode.UNAVAILABLE,
  StatusCode.INTERNAL,
  StatusCode.UNKNOWN,
]);

export interface CircuitBreakerOptions {
  failureThreshold?: number;
  resetTimeoutMs?: number;
  halfOpenMaxCalls?: number;
}

export class CircuitBreakerInterceptor implements Interceptor {
  private state: State = 'closed';
  private failures = 0;
  private openedAt = 0;
  private halfOpenCalls = 0;

  private readonly failureThreshold: number;
  private readonly resetTimeoutMs: number;
  private readonly halfOpenMaxCalls: number;

  constructor(opts: CircuitBreakerOptions = {}) {
    this.failureThreshold = opts.failureThreshold ?? 5;
    this.resetTimeoutMs = opts.resetTimeoutMs ?? 30_000;
    this.halfOpenMaxCalls = opts.halfOpenMaxCalls ?? 1;
  }

  async onRequest(_ctx: CallContext, request: unknown): Promise<unknown> {
    this.beforeCall();
    return request;
  }

  async onResponse(_ctx: CallContext, response: unknown): Promise<unknown> {
    this.recordSuccess();
    return response;
  }

  async onError(_ctx: CallContext, error: RpcError): Promise<RpcError> {
    this.recordFailure(error);
    return error;
  }

  /** Current circuit state. */
  get currentState(): State {
    return this.state;
  }

  /** Half-open probe slots currently taken. */
  get halfOpenInFlight(): number {
    return this.halfOpenCalls;
  }

  /**
   * Admit a call or throw UNAVAILABLE. An open circuit turns half-open once
   * `resetTimeoutMs` has passed; a half-open circuit admits up to
   * `halfOpenMaxCalls` probes.
   */
  beforeCall(): void {
    if (this.state === 'open') {
      if (Date.now() - this.openedAt >= this.resetTimeoutMs) {
        this.state = 'half_open';
        this.halfOpenCalls = 0;
      } else {
        throw new RpcError(StatusCode.UNAVAILABLE, 'circuit breaker is open');
      }
    }
    if (this.state === 'half_open') {
      if (this.halfOpenCalls >= this.halfOpenMaxCalls) {
        throw new RpcError(StatusCode.UNAVAILABLE, 'circuit breaker is half-open');
      }
      this.halfOpenCalls++;
    }
  }

  /** Record a successful call: closes the circuit. */
  recordSuccess(): void {
    this.failures = 0;
    this.halfOpenCalls = 0;
    this.state = 'closed';
  }

  /**
   * Record a failed call. Only UNAVAILABLE / INTERNAL / UNKNOWN count; any
   * other code gives back a half-open probe slot. Without an error (legacy
   * callers) the failure counts.
   */
  recordFailure(error?: RpcError): void {
    if (error && !COUNTED_CODES.has(error.code)) {
      this.releaseProbe();
      return;
    }
    if (this.state === 'half_open') {
      this.trip();
      return;
    }
    this.failures++;
    if (this.failures >= this.failureThreshold) this.trip();
  }

  /**
   * Give back a half-open probe slot taken by {@link beforeCall} for a call
   * whose outcome says nothing about the peer (cancelled, refused by another
   * breaker). Without this the breaker stays half-open with no free slot and
   * refuses every later call.
   */
  releaseProbe(): void {
    if (this.state === 'half_open' && this.halfOpenCalls > 0) this.halfOpenCalls--;
  }

  private trip(): void {
    this.state = 'open';
    this.openedAt = Date.now();
    this.halfOpenCalls = 0;
  }
}
