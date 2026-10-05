/**
 * Client interceptors.
 *
 * `createClient(Service, transport, { interceptors })` (and
 * `AsterClientWrapper` with `interceptors`) takes the same {@link Interceptor}
 * values a server does, with the semantics of the Python client
 * (`bindings/python/aster/client.py`) and the Go / .NET / Java ports:
 *
 * - `onRequest` runs in order on each request message, `onResponse` in order
 *   on each response message, `onError` in reverse order on a failed call (a
 *   `null` return swallows the error: a unary or client-stream call then
 *   resolves `undefined`, a stream just ends). The {@link CallContext}
 *   carries service, method, pattern, metadata (hooks may add to `metadata`:
 *   the call is sent with it), the deadline, `idempotent` and the attempt.
 * - Unary and client-streaming calls run through the retry loop: per attempt
 *   each {@link CircuitBreakerInterceptor} is consulted first (an open breaker
 *   fails the call with UNAVAILABLE, never retried), the request chain runs,
 *   the call is made, the response chain runs, and the breaker records the
 *   outcome (only UNAVAILABLE / INTERNAL / UNKNOWN count). A failed attempt
 *   goes through the `onError` chain; then a {@link RetryInterceptor} retries
 *   it if the method is idempotent, the code is retryable and attempts
 *   remain, after its backoff. Several retry interceptors retry if any does,
 *   with the longest delay and each one's own attempt cap.
 * - Server-streaming and bidi calls get the hooks only: no retry and no
 *   breaker (nothing is replayable once messages flowed).
 * - Breakers and retry interceptors are policies driven by the loop above,
 *   not hooks: their `onRequest` / `onResponse` / `onError` do not run.
 * - A client-streaming retry iterates the request iterable again, so it must
 *   be replayable (an array or a re-iterable object is; a generator object
 *   is not). Only mark methods idempotent when they are safe to replay.
 * - The deadline is sent natively on every attempt (a retry sends what is
 *   left of it) and a retry never sleeps past it; the call's `signal` also
 *   ends a backoff.
 */

import {
  applyErrorInterceptors,
  applyRequestInterceptors,
  applyResponseInterceptors,
  buildCallContext,
  normalizeError,
  type CallContext,
  type Interceptor,
} from './interceptors/base.js';
import { CircuitBreakerInterceptor } from './interceptors/circuit-breaker.js';
import { RetryInterceptor } from './interceptors/retry.js';
import { RpcError, StatusCode } from './status.js';
import type { AsterTransport, BidiChannel, CallOptions } from './transport/base.js';
import { RpcPattern } from './types.js';

/** What the chain needs to know about the method being called. */
export interface ClientMethod {
  service: string;
  method: string;
  pattern: RpcPattern;
  idempotent: boolean;
}

/** The interceptors of a client, sorted by role. */
export class ClientChain {
  private readonly hooks: Interceptor[];
  private readonly retries: RetryInterceptor[];
  private readonly breakers: CircuitBreakerInterceptor[];

  constructor(interceptors: readonly Interceptor[]) {
    this.hooks = interceptors.filter(
      i => !(i instanceof CircuitBreakerInterceptor) && !(i instanceof RetryInterceptor),
    );
    this.retries = interceptors.filter((i): i is RetryInterceptor => i instanceof RetryInterceptor);
    this.breakers = interceptors.filter(
      (i): i is CircuitBreakerInterceptor => i instanceof CircuitBreakerInterceptor,
    );
  }

  /** True when there is nothing to apply (the client takes the plain path). */
  get empty(): boolean {
    return this.hooks.length === 0 && this.retries.length === 0 && this.breakers.length === 0;
  }

  unary(transport: AsterTransport, m: ClientMethod, request: unknown, opts: CallOptions): Promise<unknown> {
    const ctx = this.context(m, opts);
    return this.retryLoop(ctx, opts, async () => {
      const req = await applyRequestInterceptors(this.hooks, ctx, request);
      const res = await transport.unary(m.service, m.method, req, attemptOptions(opts, ctx));
      return applyResponseInterceptors(this.hooks, ctx, res);
    });
  }

  clientStream(
    transport: AsterTransport,
    m: ClientMethod,
    requests: AsyncIterable<unknown>,
    opts: CallOptions,
  ): Promise<unknown> {
    const ctx = this.context(m, opts);
    const hooks = this.hooks;
    return this.retryLoop(ctx, opts, async () => {
      async function* wrapped(): AsyncIterable<unknown> {
        for await (const item of requests) yield await applyRequestInterceptors(hooks, ctx, item);
      }
      const res = await transport.clientStream(m.service, m.method, wrapped(), attemptOptions(opts, ctx));
      return applyResponseInterceptors(this.hooks, ctx, res);
    });
  }

  serverStream(
    transport: AsterTransport,
    m: ClientMethod,
    request: unknown,
    opts: CallOptions,
  ): AsyncIterable<unknown> {
    const ctx = this.context(m, opts);
    const hooks = this.hooks;
    return (async function* () {
      const req = await applyRequestInterceptors(hooks, ctx, request);
      try {
        for await (const item of transport.serverStream(m.service, m.method, req, attemptOptions(opts, ctx))) {
          yield await applyResponseInterceptors(hooks, ctx, item);
        }
      } catch (e) {
        const error = await applyErrorInterceptors(hooks, ctx, normalizeError(e));
        if (error !== null) throw error;
      }
    })();
  }

  bidi(transport: AsterTransport, m: ClientMethod, opts: CallOptions): BidiChannel {
    const ctx = this.context(m, opts);
    const inner = transport.bidiStream(m.service, m.method, attemptOptions(opts, ctx));
    if (this.hooks.length === 0) return inner;
    return new InterceptedBidiChannel(inner, this.hooks, ctx);
  }

  private context(m: ClientMethod, opts: CallOptions): CallContext {
    return buildCallContext({
      service: m.service,
      method: m.method,
      metadata: { ...opts.metadata },
      deadlineSecs: opts.deadlineSecs,
      isStreaming: m.pattern !== RpcPattern.UNARY,
      pattern: m.pattern,
      idempotent: m.idempotent,
    });
  }

  private async retryLoop(
    ctx: CallContext,
    opts: CallOptions,
    attempt: () => Promise<unknown>,
  ): Promise<unknown> {
    const maxAttempts = Math.max(1, ...this.retries.map(r => r.maxAttempts));
    let lastError: RpcError | undefined;
    for (let n = 1; n <= maxAttempts; n++) {
      ctx.attempt = n;
      this.admit();
      let response: unknown;
      try {
        response = await attempt();
      } catch (e) {
        const error = normalizeError(e);
        for (const b of this.breakers) b.recordFailure(error);
        const handled = await applyErrorInterceptors(this.hooks, ctx, error);
        if (handled === null) return undefined;
        lastError = handled;
        let retry = false;
        let delayMs = 0;
        for (const r of this.retries) {
          if (r.shouldRetry(handled, n, ctx.idempotent)) {
            retry = true;
            delayMs = Math.max(delayMs, r.backoffSeconds(n) * 1000);
          }
        }
        if (!retry) throw handled;
        await backoff(delayMs, ctx, opts.signal);
        continue;
      }
      for (const b of this.breakers) b.recordSuccess();
      return response;
    }
    throw lastError ?? new RpcError(StatusCode.INTERNAL, 'call failed');
  }

  /** Consult every breaker; if one refuses, give back the slots the others took. */
  private admit(): void {
    const admitted: CircuitBreakerInterceptor[] = [];
    try {
      for (const b of this.breakers) {
        b.beforeCall();
        admitted.push(b);
      }
    } catch (e) {
      for (const b of admitted) b.releaseProbe();
      throw e;
    }
  }
}

/** The options for one attempt: the hooks' metadata and, on a retry, what is left of the deadline. */
function attemptOptions(opts: CallOptions, ctx: CallContext): CallOptions {
  const out: CallOptions = { ...opts, metadata: ctx.metadata };
  const remaining = ctx.remainingSeconds;
  if (remaining !== undefined && ctx.attempt > 1) {
    if (remaining <= 0) throw new RpcError(StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
    out.deadlineSecs = remaining;
  }
  return out;
}

/** Sleep for a retry's backoff, never past the deadline, ending early on abort. */
function backoff(delayMs: number, ctx: CallContext, signal: AbortSignal | undefined): Promise<void> {
  const remaining = ctx.remainingSeconds;
  if (remaining !== undefined && remaining * 1000 <= delayMs) {
    return Promise.reject(new RpcError(StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded'));
  }
  if (signal?.aborted) return Promise.reject(new RpcError(StatusCode.CANCELLED, 'cancelled'));
  return new Promise((resolve, reject) => {
    const onAbort = () => {
      clearTimeout(timer);
      reject(new RpcError(StatusCode.CANCELLED, 'cancelled'));
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener('abort', onAbort);
      resolve();
    }, delayMs);
    signal?.addEventListener('abort', onAbort, { once: true });
  });
}

/** A bidi channel whose messages run through the hooks. */
class InterceptedBidiChannel implements BidiChannel {
  constructor(
    private readonly inner: BidiChannel,
    private readonly hooks: Interceptor[],
    private readonly ctx: CallContext,
  ) {}

  async send(msg: unknown): Promise<void> {
    await this.inner.send(await applyRequestInterceptors(this.hooks, this.ctx, msg));
  }

  async *[Symbol.asyncIterator](): AsyncIterator<unknown> {
    try {
      for await (const item of this.inner) {
        yield await applyResponseInterceptors(this.hooks, this.ctx, item);
      }
    } catch (e) {
      const error = await applyErrorInterceptors(this.hooks, this.ctx, normalizeError(e));
      if (error !== null) throw error;
    }
  }

  close(): Promise<void> {
    return this.inner.close();
  }

  waitForTrailer(): ReturnType<BidiChannel['waitForTrailer']> {
    return this.inner.waitForTrailer();
  }

  get cancel(): BidiChannel['cancel'] {
    return this.inner.cancel?.bind(this.inner);
  }
}
