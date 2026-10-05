/**
 * O7: tracing, metrics and the Rust health listener (design §4.2, §5.3, §6).
 *
 * One process-wide pipeline exporting over OTLP/HTTP to a collector this file
 * runs on 127.0.0.1:
 * - the AsyncLocalStorage trace carrier and `tracing.withSpan`;
 * - a dispatched call carries the server span's `traceparent`, and a client
 *   call made inside a binding span joins its trace;
 * - `HealthServer`: `/healthz`, `/readyz` with named checks, `/metrics` with
 *   custom instruments;
 * - `beforeExit` flushes spans and metrics (the batch delay is 5 s, so only
 *   the flush can have exported them).
 */

import { createServer, request as httpRequest, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import {
  initObservability,
  shutdownObservability,
  metrics,
  tracing,
  Node,
  prometheusText,
  parseTraceparent,
  formatTraceparent,
  currentTraceId,
  currentSpanId,
  currentBaggage,
  currentTraceparent,
  withTraceContext,
  traceMetadata,
  tracingMixin,
  winstonTracingFormat,
} from '@aster-rpc/aster';
import {
  DispatchLoop,
  DispatchEventKind,
  respondCall,
  type DispatchEvent,
  type DynamicServiceSpec,
} from '../../../bindings/typescript/packages/aster/src/dispatch-server.ts';
import { loadNative } from '../../../bindings/typescript/packages/aster/src/native.ts';
import { IrohTransport } from '../../../bindings/typescript/packages/aster/src/transport/iroh.ts';

const native = loadNative() as any;
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' } as const;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function until(cond: () => boolean, ms = 15_000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await sleep(10);
  }
}

// ── a local OTLP/HTTP collector ──────────────────────────────────────────────

interface Hit { path: string; body: Buffer }
const hits: Hit[] = [];
let collector: Server;
let endpoint = '';

const received = (path: string, needle: string) =>
  hits.some((h) => h.path === path && h.body.includes(Buffer.from(needle)));

function get(url: string): Promise<{ status: number; type: string; body: string }> {
  return new Promise((resolve, reject) => {
    httpRequest(url, (res) => {
      const chunks: Buffer[] = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () =>
        resolve({
          status: res.statusCode ?? 0,
          type: String(res.headers['content-type'] ?? ''),
          body: Buffer.concat(chunks).toString(),
        }),
      );
    })
      .on('error', reject)
      .end();
  });
}

beforeAll(async () => {
  collector = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      hits.push({ path: req.url ?? '', body: Buffer.concat(chunks) });
      res.writeHead(200, { 'content-type': 'application/x-protobuf' });
      res.end();
    });
  });
  await new Promise<void>((r) => collector.listen(0, '127.0.0.1', r));
  endpoint = `http://127.0.0.1:${(collector.address() as AddressInfo).port}`;
  await initObservability({
    fromEnv: false,
    serviceName: 'o7-test',
    otlpEndpoint: endpoint,
    exportTraces: true,
    exportMetrics: true,
    // The `rpc` span is INFO under `aster`: its filter defaults to the log filter.
    logFilter: 'warn,aster=info',
  });
});

afterAll(async () => {
  await shutdownObservability();
  await new Promise((r) => collector.close(r));
});

// ── the carrier ──────────────────────────────────────────────────────────────

const TP = '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01';

describe('trace-context carrier', () => {
  it('parses and formats W3C traceparent, rejecting malformed values', () => {
    const c = parseTraceparent(TP)!;
    expect(c.traceId).toBe('0af7651916cd43dd8448eb211c80319c');
    expect(c.spanId).toBe('b7ad6b7169203331');
    expect(c.traceFlags).toBe(1);
    expect(formatTraceparent(c)).toBe(TP);
    expect(parseTraceparent('garbage')).toBeUndefined();
    expect(parseTraceparent(`00-${'0'.repeat(32)}-b7ad6b7169203331-01`)).toBeUndefined();
    expect(parseTraceparent('')).toBeUndefined();
  });

  it('the four primitives follow withTraceContext, across awaits', async () => {
    expect(currentTraceId()).toBeUndefined();
    expect(currentBaggage()).toEqual({});
    await withTraceContext(TP, async () => {
      expect(currentTraceId()).toBe('0af7651916cd43dd8448eb211c80319c');
      await sleep(5);
      expect(currentSpanId()).toBe('b7ad6b7169203331');
      expect(currentTraceparent()).toBe(TP);
      expect(tracingMixin()).toEqual({ trace_id: '0af7651916cd43dd8448eb211c80319c', span_id: 'b7ad6b7169203331' });
    }, { tenant: 'acme corp' });
    expect(currentTraceId()).toBeUndefined();
    expect(tracingMixin()).toEqual({});
    // A malformed value runs the callback unchanged.
    expect(withTraceContext('nope', () => currentTraceId())).toBeUndefined();
  });

  it('traceMetadata carries traceparent and baggage for outbound calls', () => {
    expect(traceMetadata()).toEqual({});
    withTraceContext(TP, () => {
      expect(traceMetadata()).toEqual({
        traceparent: TP,
        baggage: 'tenant=acme%20corp',
      });
    }, { tenant: 'acme corp' });
  });

  it('winstonTracingFormat stamps ids through the supplied format factory', () => {
    const factory = ((fn: (i: Record<string, unknown>) => unknown) => fn) as any;
    const transform = winstonTracingFormat(factory) as (i: Record<string, unknown>) => Record<string, unknown>;
    expect(transform({ message: 'x' })).toEqual({ message: 'x' });
    withTraceContext(TP, () => {
      expect(transform({ message: 'x' })).toEqual({
        message: 'x',
        trace_id: '0af7651916cd43dd8448eb211c80319c',
        span_id: 'b7ad6b7169203331',
      });
    });
  });
});

describe('binding spans', () => {
  it('withSpan makes the span current; children share the trace; the carrier is restored', () => {
    const out = tracing.withSpan('o7.outer', { 'o7.attr': 1 }, (outer) => {
      expect(outer.context).toBeDefined();
      expect(currentTraceId()).toBe(outer.context!.traceId);
      expect(currentSpanId()).toBe(outer.context!.spanId);
      return tracing.withSpan('o7.inner', (inner) => {
        expect(inner.context!.traceId).toBe(outer.context!.traceId);
        expect(inner.context!.spanId).not.toBe(outer.context!.spanId);
        expect(currentSpanId()).toBe(inner.context!.spanId);
        inner.setAttribute('k', 'v').addEvent('evt', { n: 2 }).setStatus('ok');
        return 'done';
      });
    });
    expect(out).toBe('done');
    expect(currentTraceId()).toBeUndefined();
  });

  it('withSpanAsync keeps the carrier over awaits and records a thrown error', async () => {
    let tid = '';
    await tracing.withSpanAsync('o7.async', async (s) => {
      tid = s.context!.traceId;
      await sleep(5);
      expect(currentTraceId()).toBe(tid);
    });
    await expect(
      tracing.withSpanAsync('o7.fails', async () => {
        throw new Error('boom');
      }),
    ).rejects.toThrow('boom');
    expect(currentTraceId()).toBeUndefined();
  });

  it('a span parents on an explicit traceparent', () => {
    withTraceContext(TP, () => {
      const s = tracing.span('o7.child-of-remote');
      expect(s.context!.traceId).toBe('0af7651916cd43dd8448eb211c80319c');
      expect(s.context!.spanId).not.toBe('b7ad6b7169203331');
      s.end();
      s.end(); // idempotent
    });
  });
});

// ── a dispatched call carries the server span ───────────────────────────────

const h = (b: string) => b.repeat(32);
const ECHO: DynamicServiceSpec = {
  name: 'test.Echo',
  version: 1,
  methods: [{ name: 'echo', pattern: 'unary', request: { hash: h('a1') }, response: { hash: h('b2') } }],
};
const BYTES = { encode: (x: unknown) => x as Uint8Array, decode: (x: Uint8Array) => x };

describe('tracing across a call', () => {
  it('the handler sees the server span; a client call inside a span joins its trace', async () => {
    const seen: Array<{ traceparent?: string; flags: number; inHandler?: string }> = [];
    const loop = await DispatchLoop.start({
      config: OFFLINE as any,
      services: [ECHO],
      delivery: 'tsfn',
      builtinNodeInfo: false,
      onCall: (ev: DispatchEvent, srv) => {
        const rec: (typeof seen)[number] = { traceparent: ev.traceparent, flags: ev.flags };
        withTraceContext(ev.traceparent ?? '', () => {
          rec.inHandler = currentTraceId();
        });
        seen.push(rec);
        void respondCall(srv, ev.callId, Buffer.from('ok'));
      },
      onEvent: (ev) => void (ev.kind === DispatchEventKind.Cancelled),
    });
    const alpn = Buffer.from('aster/1');
    const node = await native.Node.start({ ...OFFLINE, alpns: [alpn] });
    try {
      const t = native.asterTicketFromString(loop.address);
      const conn = await node.connectNodeAddr(t.endpointId, alpn, t.directAddrs, undefined);
      const transport = new IrohTransport(conn, BYTES as any);

      // No active span: the server span is a root of its own trace.
      await transport.unary('test.Echo', 'echo', Buffer.from('a'));
      const root = parseTraceparent(seen[0]!.traceparent)!;
      expect(root).toBeDefined();
      expect(seen[0]!.flags & 0x400).toBe(0x400);
      expect(seen[0]!.inHandler).toBe(root.traceId);

      // Inside a binding span: same trace id on the server.
      let clientTrace = '';
      await tracing.withSpanAsync('o7.client-call', async (s) => {
        clientTrace = s.context!.traceId;
        await transport.unary('test.Echo', 'echo', Buffer.from('b'));
      });
      const joined = parseTraceparent(seen[1]!.traceparent)!;
      expect(joined.traceId).toBe(clientTrace);
      expect(joined.traceId).not.toBe(root.traceId);
      conn.close(0, 'done');
    } finally {
      await node.close();
      await loop.close();
    }
  });
});

// ── health listener, custom metrics, /metrics ───────────────────────────────

describe('HealthServer over Rust', () => {
  it('serves liveness, readiness with named checks, and Prometheus metrics', async () => {
    const node = await Node.start({ ...OFFLINE });
    const health = await node.startHealth({ port: 0 });
    try {
      const base = `http://127.0.0.1:${health.port}`;
      expect(health.addr).toBe(`127.0.0.1:${health.port}`);

      expect((await get(`${base}/healthz`)).status).toBe(200);
      expect((await get(`${base}/readyz`)).status).toBe(503); // not ready until the app says so

      const db = health.check('db'); // created failing
      health.setServices(2);
      health.setReady(true);
      let ready = await get(`${base}/readyz`);
      expect(ready.status).toBe(503);
      expect(JSON.parse(ready.body).checks).toMatchObject({ db: false });
      db.set(true);
      expect(db.ok).toBe(true);
      ready = await get(`${base}/readyz`);
      expect(ready.status).toBe(200);
      expect(JSON.parse(ready.body).services).toBe(2);

      health.setHealthy(false);
      expect((await get(`${base}/healthz`)).status).toBe(503);
      health.setHealthy(true);

      // Custom instruments are exported with the built-ins.
      const reqs = metrics.counter('o7.requests', { description: 'Test requests' });
      const depth = metrics.upDownCounter('o7.depth');
      const lat = metrics.histogram('o7.latency', { unit: 's' });
      const temp = metrics.gauge('o7.temp');
      const hot = metrics.attributes({ route: '/a' });
      reqs.add(1, { route: '/a' });
      reqs.add(2, hot);
      depth.add(5);
      depth.add(-2);
      lat.record(0.25, { route: '/a' });
      temp.record(21.5);
      expect(() => reqs.add(-1)).toThrow(/non-negative/);

      const m = await get(`${base}/metrics`);
      expect(m.status).toBe(200);
      expect(m.type).toContain('text/plain');
      expect(m.body).toMatch(/^o7_requests_total\{[^}]*route="\/a"[^}]*\} 3$/m);
      expect(m.body).toMatch(/^o7_depth(\{[^}]*\})? 3$/m);
      expect(m.body).toMatch(/^o7_latency_seconds_count\{[^}]*\} 1$/m);
      expect(m.body).toMatch(/^o7_temp(\{[^}]*\})? 21.5$/m);
      expect(m.body).toContain('process_uptime_seconds');
      expect(prometheusText()).toContain('o7_requests_total');

      // Same name, other kind: refused in Rust.
      expect(() => metrics.histogram('o7.requests')).toThrow();
      expect((await get(`${base}/nope`)).status).toBe(404);
    } finally {
      await health.stop();
      await node.close();
    }
    // The port is released.
    await expect(get(`http://127.0.0.1:${health.port}/healthz`)).rejects.toThrow();
  });
});

// ── flush on beforeExit ─────────────────────────────────────────────────────

describe('flush on beforeExit', () => {
  it('exports spans and metrics that were still batched', async () => {
    const counter = metrics.counter('o7.before_exit');
    tracing.withSpan('o7.beforeexit.span', () => counter.add(7, { phase: 'exit' }));
    hits.length = 0;
    await sleep(300);
    // The batch delay is 5 s and the metric interval 60 s: nothing yet.
    expect(received('/v1/traces', 'o7.beforeexit.span')).toBe(false);

    process.emit('beforeExit', 0);
    await until(() => received('/v1/traces', 'o7.beforeexit.span'));
    await until(() => received('/v1/metrics', 'o7.before_exit'));
    expect(received('/v1/traces', 'o7-test')).toBe(true); // resource service.name
  });
});
