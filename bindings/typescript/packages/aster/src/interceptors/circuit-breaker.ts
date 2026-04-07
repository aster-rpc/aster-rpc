/**
 * Circuit breaker interceptor — stops sending requests to failing services.
 *
 * States: CLOSED (normal) -> OPEN (failing) -> HALF_OPEN (probe) -> CLOSED
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';
import { RpcError, StatusCode } from '../status.js';

type State = 'closed' | 'open' | 'half_open';

export interface CircuitBreakerOptions {
  failureThreshold?: number;
  resetTimeoutMs?: number;
  halfOpenMaxCalls?: number;
}

export class CircuitBreakerInterceptor implements Interceptor {
  private state: State = 'closed';
  private failures = 0;
  private lastFailure = 0;
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
    if (this.state === 'open') {
      if (Date.now() - this.lastFailure > this.resetTimeoutMs) {
        this.state = 'half_open';
        this.halfOpenCalls = 0;
      } else {
        throw new RpcError(StatusCode.UNAVAILABLE, 'circuit breaker is open');
      }
    }

    if (this.state === 'half_open' && this.halfOpenCalls >= this.halfOpenMaxCalls) {
      throw new RpcError(StatusCode.UNAVAILABLE, 'circuit breaker half-open limit reached');
    }

    if (this.state === 'half_open') this.halfOpenCalls++;
    return request;
  }

  async onResponse(_ctx: CallContext, response: unknown): Promise<unknown> {
    if (this.state === 'half_open') {
      this.state = 'closed';
      this.failures = 0;
    }
    return response;
  }

  async onError(_ctx: CallContext, error: RpcError): Promise<RpcError> {
    this.failures++;
    this.lastFailure = Date.now();
    if (this.failures >= this.failureThreshold) {
      this.state = 'open';
    }
    return error;
  }

  /** Current circuit state. */
  get currentState(): State {
    return this.state;
  }

  /**
   * Pre-call gate check — throws if circuit is open.
   * Alias for the logic in onRequest(), callable without a full call context.
   */
  beforeCall(): void {
    if (this.state === 'open') {
      if (Date.now() - this.lastFailure > this.resetTimeoutMs) {
        this.state = 'half_open';
        this.halfOpenCalls = 0;
      } else {
        throw new RpcError(StatusCode.UNAVAILABLE, 'circuit breaker is open');
      }
    }
    if (this.state === 'half_open' && this.halfOpenCalls >= this.halfOpenMaxCalls) {
      throw new RpcError(StatusCode.UNAVAILABLE, 'circuit breaker half-open limit reached');
    }
    if (this.state === 'half_open') this.halfOpenCalls++;
  }

  /** Record a successful call — resets the failure count. */
  recordSuccess(): void {
    if (this.state === 'half_open') this.state = 'closed';
    this.failures = 0;
  }

  /** Record a failed call — may open the circuit. */
  recordFailure(): void {
    this.failures++;
    this.lastFailure = Date.now();
    if (this.failures >= this.failureThreshold) {
      this.state = 'open';
    }
  }
}
