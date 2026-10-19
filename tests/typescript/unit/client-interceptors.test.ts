import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  createClient,
  CircuitBreakerInterceptor,
  RetryInterceptor,
  RpcError,
  StatusCode,
  type CallContext,
  type Interceptor,
} from '@aster-rpc/aster';

// -- Test service (the client only reads its metadata) -------------------------

@Service({ name: 'Flaky', version: 1 })
class FlakyService {
  @Rpc()
  async call(_req: { v: string }): Promise<{ v: string }> { throw new Error('client only'); }

  @Rpc({ idempotent: true })
  async idem(_req: { v: string }): Promise<{ v: string }> { throw new Error('client only'); }

  @ServerStream()
  async *watch(_req: { v: string }): AsyncGenerator<{ v: string }> { throw new Error('client only'); }

  @ClientStream({ idempotent: true })
  async ingest(_reqs: AsyncIterable<{ v: string }>): Promise<{ v: string }> { throw new Error('client only'); }

  @BidiStream()
  async *chat(_reqs: AsyncIterable<{ v: string }>): AsyncGenerator<{ v: string }> { throw new Error('client only'); }
}

/** A step of the scripted transport: an error to throw, or a reply value. */
type Step = RpcError | string;

interface Sent { method: string; request: unknown; metadata?: Record<string, string>; deadlineSecs?: number }

function scripted(script: Step[], streamItems: string[] = [], streamError?: RpcError) {
  const sent: Sent[] = [];
  const reply = (method: string, request: unknown, opts: any) => {
    sent.push({ method, request, metadata: opts?.metadata, deadlineSecs: opts?.deadlineSecs });
    const step = script.shift();
    if (step === undefined) throw new Error(`unscripted call to ${method}`);
    if (step instanceof RpcError) throw step;
    return { v: step };
  };
  const transport = {
    async unary(_s: string, method: string, request: unknown, opts?: any) {
      return reply(method, request, opts);
    },
    serverStream(_s: string, method: string, request: unknown, opts?: any): AsyncIterable<unknown> {
      sent.push({ method, request, metadata: opts?.metadata });
      return (async function* () {
        for (const v of streamItems) yield { v };
        if (streamError) throw streamError;
      })();
    },
    async clientStream(_s: string, method: string, requests: AsyncIterable<unknown>, opts?: any) {
      const items: unknown[] = [];
      for await (const r of requests) items.push(r);
      return reply(method, items, opts);
    },
    bidiStream(_s: string, method: string, opts?: any) {
      const queue: unknown[] = [];
      sent.push({ method, request: queue, metadata: opts?.metadata });
      return {
        async send(msg: unknown) { queue.push(msg); },
        async *[Symbol.asyncIterator]() {
          for (const m of queue) yield { v: `echo:${(m as { v: string }).v}` };
        },
        async close() {},
        async waitForTrailer(): Promise<[StatusCode, string]> { return [StatusCode.OK, '']; },
      };
    },
    async close() {},
  };
  return { transport, sent };
}

// Retry backoff and breaker reset run on `setTimeout` and `Date.now`; vitest's
// fake timers replace both, so these tests run on a virtual clock with no real
// sleeps and can assert exact deadlines.
beforeEach(() => { vi.useFakeTimers({ now: 1_000_000 }); });
afterEach(() => { vi.useRealTimers(); });

/** Settle `p`, firing pending timers in order on the virtual clock. */
async function drive<T>(p: Promise<T>): Promise<T> {
  let done = false;
  const outcome = p.then(
    (value) => { done = true; return { ok: true as const, value }; },
    (error: unknown) => { done = true; return { ok: false as const, error }; },
  );
  for (let i = 0; i < 100 && !done; i++) await vi.advanceTimersToNextTimerAsync();
  const r = await outcome;
  if (r.ok) return r.value;
  throw r.error;
}

const unavailable = () => new RpcError(StatusCode.UNAVAILABLE, 'down');
const notFound = () => new RpcError(StatusCode.NOT_FOUND, 'missing');
const fastRetry = (maxAttempts: number) =>
  new RetryInterceptor({ maxAttempts, backoff: { initialMs: 1, multiplier: 1, maxMs: 1, jitter: 0 } });

/** Records hook calls; tags requests, responses and metadata. */
class Tagger implements Interceptor {
  log: string[] = [];
  constructor(private readonly tag: string) {}
  async onRequest(ctx: CallContext, request: unknown) {
    this.log.push(`req:${ctx.method}:${ctx.attempt}`);
    ctx.metadata[`x-${this.tag}`] = '1';
    return { v: `${(request as { v: string }).v}+${this.tag}` };
  }
  async onResponse(_ctx: CallContext, response: unknown) {
    this.log.push('resp');
    return { v: `${(response as { v: string }).v}+${this.tag}` };
  }
  async onError(_ctx: CallContext, error: RpcError) {
    this.log.push(`err:${error.code}`);
    return error;
  }
}

describe('client interceptors: hooks', () => {
  it('unary: request and response chains in order, metadata sent', async () => {
    const { transport, sent } = scripted(['ok']);
    const a = new Tagger('a');
    const b = new Tagger('b');
    const client = createClient(FlakyService, transport as any, { interceptors: [a, b], metadata: { base: 'y' } });
    const out = await client.call({ v: 'r' });
    expect(sent[0]!.request).toEqual({ v: 'r+a+b' });
    expect(sent[0]!.metadata).toEqual({ base: 'y', 'x-a': '1', 'x-b': '1' });
    expect(out).toEqual({ v: 'ok+a+b' });
  });

  it('onError returning null swallows the error', async () => {
    const { transport } = scripted([unavailable()]);
    const swallow: Interceptor = { onError: async () => null };
    const client = createClient(FlakyService, transport as any, { interceptors: [swallow] });
    await expect(client.call({ v: 'r' })).resolves.toBeUndefined();
  });

  it('server stream: request hook once, response hook per item, error chain at the end', async () => {
    const { transport, sent } = scripted([], ['1', '2'], notFound());
    const t = new Tagger('t');
    const client = createClient(FlakyService, transport as any, { interceptors: [t] });
    const got: unknown[] = [];
    await expect((async () => {
      for await (const item of client.watch({ v: 'w' }) as AsyncIterable<unknown>) got.push(item);
    })()).rejects.toMatchObject({ code: StatusCode.NOT_FOUND });
    expect(sent[0]!.request).toEqual({ v: 'w+t' });
    expect(got).toEqual([{ v: '1+t' }, { v: '2+t' }]);
    expect(t.log).toEqual(['req:watch:1', 'resp', 'resp', `err:${StatusCode.NOT_FOUND}`]);
  });

  it('bidi: send and receive run through the hooks', async () => {
    const { transport } = scripted([]);
    const t = new Tagger('t');
    const client = createClient(FlakyService, transport as any, { interceptors: [t] });
    const ch = client.chat() as any;
    await ch.send({ v: 'x' });
    const got: unknown[] = [];
    for await (const item of ch) got.push(item);
    expect(got).toEqual([{ v: 'echo:x+t+t' }]);
  });
});

describe('client interceptors: retry', () => {
  it('retries an idempotent unary call on UNAVAILABLE, without running retry as a hook', async () => {
    const { transport, sent } = scripted([unavailable(), unavailable(), 'ok']);
    const t = new Tagger('t');
    const client = createClient(FlakyService, transport as any, { interceptors: [t, fastRetry(3)] });
    expect(await drive(client.idem({ v: 'r' }))).toEqual({ v: 'ok+t' });
    expect(sent).toHaveLength(3);
    expect(t.log.filter(l => l.startsWith('req'))).toEqual(['req:idem:1', 'req:idem:2', 'req:idem:3']);
  });

  it('does not retry a non-idempotent call; error details carry no retry hints', async () => {
    const { transport, sent } = scripted([unavailable()]);
    const client = createClient(FlakyService, transport as any, { interceptors: [fastRetry(3)] });
    const err = await client.call({ v: 'r' }).catch((e: RpcError) => e);
    expect(err).toBeInstanceOf(RpcError);
    expect((err as RpcError).details['retry_after_ms']).toBeUndefined();
    expect(sent).toHaveLength(1);
  });

  it('client stream: per-message request hooks and a retry replays the requests', async () => {
    const { transport, sent } = scripted([unavailable(), 'ok']);
    const t = new Tagger('t');
    const client = createClient(FlakyService, transport as any, { interceptors: [t, fastRetry(2)] });
    const reqs = { async *[Symbol.asyncIterator]() { yield { v: 'a' }; yield { v: 'b' }; } };
    expect(await drive(client.ingest(reqs))).toEqual({ v: 'ok+t' });
    expect(sent.map(s => s.request)).toEqual([
      [{ v: 'a+t' }, { v: 'b+t' }],
      [{ v: 'a+t' }, { v: 'b+t' }],
    ]);
  });

  it('a retry sends what is left of the deadline and never sleeps past it', async () => {
    const { transport, sent } = scripted([unavailable(), unavailable()]);
    const slow = new RetryInterceptor({ maxAttempts: 3, backoff: { initialMs: 5_000, multiplier: 1, maxMs: 5_000, jitter: 0 } });
    const client = createClient(FlakyService, transport as any, { interceptors: [slow] });
    const started = Date.now();
    const err = await drive(client.idem({ v: 'r' }, { deadlineSecs: 0.2 })).catch((e: RpcError) => e);
    expect((err as RpcError).code).toBe(StatusCode.DEADLINE_EXCEEDED);
    // The 5 s backoff cannot fit in the 0.2 s deadline, so no time passes at all.
    expect(Date.now() - started).toBe(0);
    expect(sent).toHaveLength(1);
    expect(sent[0]!.deadlineSecs).toBe(0.2);
  });

  it('a retry sends the remaining deadline, not the original', async () => {
    const { transport, sent } = scripted([unavailable(), 'ok']);
    const retry = new RetryInterceptor({ maxAttempts: 2, backoff: { initialMs: 50, multiplier: 1, maxMs: 50, jitter: 0 } });
    const client = createClient(FlakyService, transport as any, { interceptors: [retry] });
    expect(await drive(client.idem({ v: 'r' }, { deadlineSecs: 5 }))).toEqual({ v: 'ok' });
    expect(sent[0]!.deadlineSecs).toBe(5);
    // Exactly the 50 ms backoff has passed on the virtual clock.
    expect(sent[1]!.deadlineSecs).toBeCloseTo(4.95, 6);
  });

  it('an abort ends the backoff with CANCELLED', async () => {
    const { transport } = scripted([unavailable()]);
    const slow = new RetryInterceptor({ maxAttempts: 3, backoff: { initialMs: 5_000, multiplier: 1, maxMs: 5_000, jitter: 0 } });
    const client = createClient(FlakyService, transport as any, { interceptors: [slow] });
    const ac = new AbortController();
    const call = client.idem({ v: 'r' }, { signal: ac.signal }).catch((e: RpcError) => e);
    // 20 ms into the 5 s backoff, the abort ends it.
    await vi.advanceTimersByTimeAsync(20);
    ac.abort();
    expect(await call).toMatchObject({ code: StatusCode.CANCELLED });
    expect(vi.getTimerCount()).toBe(0);
  });
});

describe('client interceptors: circuit breaker', () => {
  const open = async (client: any, breaker: CircuitBreakerInterceptor) => {
    await expect(client.call({ v: 'trip' })).rejects.toMatchObject({ code: StatusCode.UNAVAILABLE });
    expect(breaker.currentState).toBe('open');
  };

  it('opens after counted failures and refuses without calling the transport', async () => {
    const { transport, sent } = scripted([unavailable()]);
    const breaker = new CircuitBreakerInterceptor({ failureThreshold: 1, resetTimeoutMs: 60_000 });
    const client = createClient(FlakyService, transport as any, { interceptors: [breaker] });
    await open(client, breaker);
    await expect(client.call({ v: 'r' })).rejects.toThrow('circuit breaker is open');
    expect(sent).toHaveLength(1);
  });

  it('a non-counted error does not trip it', async () => {
    const { transport } = scripted([notFound(), notFound(), 'ok']);
    const breaker = new CircuitBreakerInterceptor({ failureThreshold: 2 });
    const client = createClient(FlakyService, transport as any, { interceptors: [breaker] });
    await expect(client.call({ v: 'r' })).rejects.toMatchObject({ code: StatusCode.NOT_FOUND });
    await expect(client.call({ v: 'r' })).rejects.toMatchObject({ code: StatusCode.NOT_FOUND });
    expect(breaker.currentState).toBe('closed');
    expect(await client.call({ v: 'r' })).toEqual({ v: 'ok' });
  });

  it('half-open: a non-counted error frees the probe slot; a success closes it', async () => {
    const { transport } = scripted([unavailable(), notFound(), 'ok']);
    const breaker = new CircuitBreakerInterceptor({ failureThreshold: 1, resetTimeoutMs: 30_000 });
    const client = createClient(FlakyService, transport as any, { interceptors: [breaker] });
    await open(client, breaker);
    vi.advanceTimersByTime(29_999);
    await expect(client.call({ v: 'early' })).rejects.toThrow('circuit breaker is open');
    vi.advanceTimersByTime(1);
    await expect(client.call({ v: 'probe' })).rejects.toMatchObject({ code: StatusCode.NOT_FOUND });
    expect(breaker.currentState).toBe('half_open');
    expect(breaker.halfOpenInFlight).toBe(0);
    expect(await client.call({ v: 'probe' })).toEqual({ v: 'ok' });
    expect(breaker.currentState).toBe('closed');
  });

  it('half-open: a counted failure reopens it', async () => {
    const { transport } = scripted([unavailable(), unavailable()]);
    const breaker = new CircuitBreakerInterceptor({ failureThreshold: 3, resetTimeoutMs: 30_000 });
    const client = createClient(FlakyService, transport as any, { interceptors: [breaker] });
    breaker.recordFailure(unavailable());
    breaker.recordFailure(unavailable());
    await open(client, breaker);
    vi.advanceTimersByTime(30_000);
    await expect(client.call({ v: 'probe' })).rejects.toMatchObject({ code: StatusCode.UNAVAILABLE });
    expect(breaker.currentState).toBe('open');
  });

  it('a refusal by a later breaker gives back the earlier breaker\'s probe slot', async () => {
    const { transport } = scripted([unavailable()]);
    const first = new CircuitBreakerInterceptor({ failureThreshold: 1, resetTimeoutMs: 30_000 });
    const second = new CircuitBreakerInterceptor({ failureThreshold: 1, resetTimeoutMs: 60_000 });
    const client = createClient(FlakyService, transport as any, { interceptors: [first, second] });
    await open(client, first);
    expect(second.currentState).toBe('open');
    vi.advanceTimersByTime(30_000);
    await expect(client.call({ v: 'probe' })).rejects.toThrow('circuit breaker is open');
    expect(first.currentState).toBe('half_open');
    expect(first.halfOpenInFlight).toBe(0);
  });

  it('an open breaker is not retried', async () => {
    const { transport, sent } = scripted([unavailable()]);
    const breaker = new CircuitBreakerInterceptor({ failureThreshold: 1, resetTimeoutMs: 60_000 });
    const client = createClient(FlakyService, transport as any, { interceptors: [breaker, fastRetry(3)] });
    await expect(drive(client.idem({ v: 'r' }))).rejects.toThrow('circuit breaker is open');
    expect(sent).toHaveLength(1);
  });
});
