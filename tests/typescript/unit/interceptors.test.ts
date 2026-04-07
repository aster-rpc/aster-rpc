import { describe, it, expect, vi } from 'vitest';
import {
  CallContext,
  applyRequestInterceptors,
  applyResponseInterceptors,
  applyErrorInterceptors,
  normalizeError,
  DeadlineInterceptor,
  MetricsInterceptor,
  RetryInterceptor,
  RateLimitInterceptor,
  AuthInterceptor,
  CircuitBreakerInterceptor,
  AuditLogInterceptor,
  CapabilityInterceptor,
  RpcError,
  StatusCode,
  type Interceptor,
} from '@aster-rpc/aster';

function ctx(overrides?: Partial<ConstructorParameters<typeof CallContext>[0]>): CallContext {
  return new CallContext({ service: 'Test', method: 'test', ...overrides });
}

// -- CallContext ---------------------------------------------------------------

describe('CallContext', () => {
  it('generates a callId by default', () => {
    const c = ctx();
    expect(c.callId).toBeTruthy();
    expect(c.callId.length).toBeGreaterThan(0);
  });

  it('computes remainingSeconds', () => {
    const c = ctx({ deadline: Date.now() / 1000 + 10 });
    expect(c.remainingSeconds).toBeGreaterThan(8);
    expect(c.remainingSeconds).toBeLessThanOrEqual(10);
    expect(c.expired).toBe(false);
  });

  it('detects expired deadline', () => {
    const c = ctx({ deadline: Date.now() / 1000 - 1 });
    expect(c.expired).toBe(true);
    expect(c.remainingSeconds).toBe(0);
  });

  it('returns undefined for no deadline', () => {
    const c = ctx();
    expect(c.remainingSeconds).toBeUndefined();
    expect(c.expired).toBe(false);
  });
});

// -- Interceptor chain --------------------------------------------------------

describe('interceptor chain', () => {
  it('applies request interceptors in order', async () => {
    const order: number[] = [];
    const i1: Interceptor = { async onRequest(_, req) { order.push(1); return req; } };
    const i2: Interceptor = { async onRequest(_, req) { order.push(2); return req; } };
    await applyRequestInterceptors([i1, i2], ctx(), {});
    expect(order).toEqual([1, 2]);
  });

  it('applies error interceptors in reverse order', async () => {
    const order: number[] = [];
    const i1: Interceptor = { async onError(_, e) { order.push(1); return e; } };
    const i2: Interceptor = { async onError(_, e) { order.push(2); return e; } };
    await applyErrorInterceptors([i1, i2], ctx(), new RpcError(StatusCode.INTERNAL));
    expect(order).toEqual([2, 1]);
  });

  it('stops on null from error interceptor', async () => {
    const i1: Interceptor = { async onError() { return null; } };
    const i2: Interceptor = { async onError(_, e) { return e; } };
    const result = await applyErrorInterceptors([i1, i2], ctx(), new RpcError(StatusCode.INTERNAL));
    expect(result).toBeNull();
  });
});

// -- normalizeError -----------------------------------------------------------

describe('normalizeError', () => {
  it('passes through RpcError', () => {
    const err = new RpcError(StatusCode.NOT_FOUND, 'gone');
    expect(normalizeError(err)).toBe(err);
  });

  it('wraps generic Error as UNKNOWN', () => {
    const err = normalizeError(new Error('oops'));
    expect(err.code).toBe(StatusCode.UNKNOWN);
    expect(err.message).toContain('oops');
  });
});

// -- DeadlineInterceptor ------------------------------------------------------

describe('DeadlineInterceptor', () => {
  it('passes non-expired deadline', async () => {
    const di = new DeadlineInterceptor();
    const c = ctx({ deadline: Date.now() / 1000 + 60 });
    const result = await di.onRequest(c, 'req');
    expect(result).toBe('req');
  });

  it('rejects expired deadline', async () => {
    const di = new DeadlineInterceptor();
    const c = ctx({ deadline: Date.now() / 1000 - 10 });
    await expect(di.onRequest(c, 'req')).rejects.toThrow('deadline');
  });

  it('computes timeout seconds', () => {
    const di = new DeadlineInterceptor();
    const c = ctx({ deadline: Date.now() / 1000 + 5 });
    const t = di.timeoutSeconds(c);
    expect(t).toBeGreaterThan(3);
    expect(t).toBeLessThanOrEqual(5);
  });
});

// -- MetricsInterceptor -------------------------------------------------------

describe('MetricsInterceptor', () => {
  it('tracks started/succeeded', async () => {
    const mi = new MetricsInterceptor();
    await mi.onRequest(ctx(), {});
    expect(mi.snapshot().started).toBe(1);
    expect(mi.snapshot().inFlight).toBe(1);
    await mi.onResponse(ctx(), {});
    expect(mi.snapshot().succeeded).toBe(1);
    expect(mi.snapshot().inFlight).toBe(0);
  });

  it('tracks failed', async () => {
    const mi = new MetricsInterceptor();
    await mi.onRequest(ctx(), {});
    await mi.onError(ctx(), new RpcError(StatusCode.INTERNAL));
    expect(mi.snapshot().failed).toBe(1);
    expect(mi.snapshot().inFlight).toBe(0);
  });

  it('resets', () => {
    const mi = new MetricsInterceptor();
    mi.started = 10;
    mi.reset();
    expect(mi.snapshot().started).toBe(0);
  });
});

// -- RetryInterceptor ---------------------------------------------------------

describe('RetryInterceptor', () => {
  it('adds retry metadata for idempotent calls on UNAVAILABLE', async () => {
    const ri = new RetryInterceptor({ maxAttempts: 3 });
    const c = ctx({ idempotent: true });
    c.attempt = 1;
    const err = new RpcError(StatusCode.UNAVAILABLE, 'down');
    const result = await ri.onError(c, err);
    expect(result!.details['retry_after_ms']).toBeTruthy();
  });

  it('does not retry non-idempotent calls', async () => {
    const ri = new RetryInterceptor();
    const c = ctx({ idempotent: false });
    const err = new RpcError(StatusCode.UNAVAILABLE, 'down');
    const result = await ri.onError(c, err);
    expect(result!.details['retry_after_ms']).toBeUndefined();
  });

  it('does not retry on non-retryable codes', async () => {
    const ri = new RetryInterceptor();
    const c = ctx({ idempotent: true });
    const err = new RpcError(StatusCode.NOT_FOUND, 'gone');
    const result = await ri.onError(c, err);
    expect(result!.details['retry_after_ms']).toBeUndefined();
  });
});

// -- RateLimitInterceptor -----------------------------------------------------

describe('RateLimitInterceptor', () => {
  it('allows requests within limit', async () => {
    const rli = new RateLimitInterceptor({ globalRps: 100 });
    const result = await rli.onRequest(ctx(), {});
    expect(result).toEqual({});
  });

  it('rejects when global limit exceeded', async () => {
    const rli = new RateLimitInterceptor({ globalRps: 1 });
    // Consume the bucket
    await rli.onRequest(ctx(), {});
    await rli.onRequest(ctx(), {}); // burst allows 2
    // Third should fail
    await expect(rli.onRequest(ctx(), {})).rejects.toThrow('rate limit');
  });
});

// -- AuthInterceptor ----------------------------------------------------------

describe('AuthInterceptor', () => {
  it('injects token from provider', async () => {
    const ai = new AuthInterceptor(() => 'token123');
    const c = ctx();
    await ai.onRequest(c, {});
    expect(c.metadata['authorization']).toBe('token123');
  });

  it('rejects missing token on validation', async () => {
    const ai = new AuthInterceptor(undefined, () => true);
    await expect(ai.onRequest(ctx(), {})).rejects.toThrow('missing auth');
  });

  it('rejects invalid token', async () => {
    const ai = new AuthInterceptor(undefined, () => false);
    const c = ctx({ metadata: { authorization: 'bad' } });
    await expect(ai.onRequest(c, {})).rejects.toThrow('invalid auth');
  });
});

// -- CircuitBreakerInterceptor ------------------------------------------------

describe('CircuitBreakerInterceptor', () => {
  it('starts closed', () => {
    const cb = new CircuitBreakerInterceptor();
    expect(cb.currentState).toBe('closed');
  });

  it('opens after threshold failures', async () => {
    const cb = new CircuitBreakerInterceptor({ failureThreshold: 2 });
    await cb.onError(ctx(), new RpcError(StatusCode.INTERNAL));
    expect(cb.currentState).toBe('closed');
    await cb.onError(ctx(), new RpcError(StatusCode.INTERNAL));
    expect(cb.currentState).toBe('open');
  });

  it('rejects requests when open', async () => {
    const cb = new CircuitBreakerInterceptor({ failureThreshold: 1 });
    await cb.onError(ctx(), new RpcError(StatusCode.INTERNAL));
    await expect(cb.onRequest(ctx(), {})).rejects.toThrow('circuit breaker is open');
  });
});

// -- CapabilityInterceptor ----------------------------------------------------

describe('CapabilityInterceptor', () => {
  it('passes when role matches', async () => {
    const ci = new CapabilityInterceptor();
    ci.setRequirement('Svc', 'method', { kind: 'role', roles: ['admin'] });
    const c = ctx({ service: 'Svc', method: 'method', attributes: { 'aster.role': 'admin' } });
    const result = await ci.onRequest(c, {});
    expect(result).toEqual({});
  });

  it('rejects when role does not match', async () => {
    const ci = new CapabilityInterceptor();
    ci.setRequirement('Svc', 'method', { kind: 'role', roles: ['admin'] });
    const c = ctx({ service: 'Svc', method: 'method', attributes: { 'aster.role': 'reader' } });
    await expect(ci.onRequest(c, {})).rejects.toThrow('PERMISSION_DENIED');
  });

  it('passes when no requirement set', async () => {
    const ci = new CapabilityInterceptor();
    const result = await ci.onRequest(ctx(), {});
    expect(result).toEqual({});
  });
});

// -- AuditLogInterceptor ------------------------------------------------------

describe('AuditLogInterceptor', () => {
  it('logs request, response, and error', async () => {
    const entries: any[] = [];
    const ali = new AuditLogInterceptor((e) => entries.push(e));
    const c = ctx();
    await ali.onRequest(c, {});
    await ali.onResponse(c, {});
    await ali.onError(c, new RpcError(StatusCode.INTERNAL, 'oops'));
    expect(entries.length).toBe(3);
    expect(entries[0].status).toBe('started');
    expect(entries[1].status).toBe('completed');
    expect(entries[2].status).toBe('failed');
    expect(entries[2].errorCode).toBe('INTERNAL');
  });
});
