/**
 * Path B (design 3.4): the application's own log records go into the Rust OTel
 * logs SDK (`emitLog`, `AsterLogger({ otlp: true })`) and are exported over
 * OTLP/HTTP to a collector this file runs on 127.0.0.1.
 */

import { createServer, type Server, type ServerResponse } from 'node:http';
import { connect } from 'node:net';
import type { AddressInfo } from 'node:net';
import { gunzipSync } from 'node:zlib';
import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { initObservability, shutdownObservability, flushObservability, ensureObservability, logStats, emitLog, AsterLogger, tracing, setLogSink, pollLogs, Node } from '@aster-rpc/aster';

const hits: { path: string; body: Buffer }[] = [];
let collector: Server;
let rejectLogs = false;
let rejectedLogs = 0;
let stallLogs = false;
let collectorPort = 0;
const stalledResponses: ServerResponse[] = [];
const options = () => ({ fromEnv: false, serviceName: 'pathb-test', otlpEndpoint: `http://127.0.0.1:${collectorPort}`,
  exportOtlpLogs: true, exportTraces: true, logFilter: 'warn,aster=info' });

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const got = (needle: string | Buffer) =>
  hits.some((h) => h.path.endsWith('/v1/logs') && h.body.includes(typeof needle === 'string' ? Buffer.from(needle) : needle));

async function until(cond: () => boolean, ms = 10_000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error('timed out waiting');
    await flushObservability();
    await sleep(50);
  }
}

async function startCollector(port = 0): Promise<void> {
  collector = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      let body = Buffer.concat(chunks);
      if (req.headers['content-encoding'] === 'gzip') body = gunzipSync(body);
      if (rejectLogs && req.url?.endsWith('/v1/logs')) {
        rejectedLogs++;
        if (stallLogs) {
          stalledResponses.push(res);
          return;
        }
        res.writeHead(503);
        res.end();
        return;
      }
      hits.push({ path: req.url ?? '', body });
      res.writeHead(200, { 'content-type': 'application/x-protobuf' });
      res.end();
    });
  });
  await new Promise<void>((r) => collector.listen(port, '127.0.0.1', r));
  collectorPort = (collector.address() as AddressInfo).port;
}

async function closeCollector(): Promise<void> {
  collector.closeAllConnections();
  await new Promise<void>((resolve, reject) => collector.close((error) => error ? reject(error) : resolve()));
}

function releaseStalls(): void {
  for (const response of stalledResponses.splice(0)) { response.writeHead(503); response.end(); }
}

beforeAll(async () => {
  await startCollector();
  await initObservability(options());
});

afterAll(async () => {
  await shutdownObservability();
  await new Promise((r) => collector.close(r));
});

const TP = '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01';

describe('emitLog (path B)', () => {
  it('exports a record with its logger, fields and the given trace ids', async () => {
    emitLog('warning', 'pathb-marker-direct', { order: 'o-1', n: 7 }, { logger: 'app.pathb', traceparent: TP });
    await until(() => got('pathb-marker-direct'));
    expect(got('app.pathb')).toBe(true);
    expect(got('order') && got('o-1')).toBe(true);
    expect(got(Buffer.from('0af7651916cd43dd8448eb211c80319c', 'hex'))).toBe(true);
    expect(got(Buffer.from('b7ad6b7169203331', 'hex'))).toBe(true);
  });

  it('attaches the current trace context, and AsterLogger exports only when opted in', async () => {
    const quiet = new AsterLogger({ format: 'json', level: 'debug' });
    const loud = new AsterLogger({ format: 'json', level: 'debug', otlp: true, name: 'app.loud' });
    const log = console.log;
    console.log = () => {};
    let traceId = '';
    try {
      await tracing.withSpan('pathb.work', async () => {
        traceId = tracing.currentTraceId()!;
        quiet.info('pathb-marker-quiet');
        loud.info('pathb-marker-loud', { secret_key: 'abc' });
      });
    } finally {
      console.log = log;
    }
    await until(() => got('pathb-marker-loud'));
    expect(got('app.loud')).toBe(true);
    expect(got(Buffer.from(traceId, 'hex'))).toBe(true);
    expect(got('pathb-marker-quiet')).toBe(false);
    expect(got('abc')).toBe(false); // masked like the printed record
  });

  it('rejects an unknown level', () => {
    expect(() => emitLog('loud' as never, 'x')).toThrow(RangeError);
  });

  it.each([
    ['http503', 'shutdown'], ['refused', 'shutdown'], ['stall', 'shutdown'],
    ['http503', 'readiness'], ['refused', 'readiness'], ['stall', 'readiness'],
  ])('keeps application emission bounded during %s exporter outage (%s) and exports after recovery', async (fault, scope) => {
    await shutdownObservability();
    await initObservability(options());
    hits.length = 0;
    const failedBefore = rejectedLogs;
    const bridgedMessages: string[] = [];
    setLogSink((record) => bridgedMessages.push(record.message));
    rejectLogs = true;
    stallLogs = fault === 'stall';
    // Readiness owns a pipeline handle for the health object's lifetime.
    // Test its isolation separately from final-reference exporter shutdown.
    const node = scope === 'readiness' ? await Node.start({ relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' }) : undefined;
    const health = await node?.startHealth({ port: 0 });
    health?.setReady(true);
    const assertReady = async () => {
      if (!health) return;
      const response = await fetch(`http://127.0.0.1:${health.port}/readyz`);
      expect(response.status).toBe(200);
      expect((await response.json() as { checks: { audit: boolean } }).checks.audit).toBe(true);
    };
    if (fault === 'refused') {
      await closeCollector();
      await new Promise<void>((resolve, reject) => {
        const socket = connect(collectorPort, '127.0.0.1');
        socket.once('connect', () => { socket.destroy(); reject(new Error('collector still listening')); });
        socket.once('error', (error: NodeJS.ErrnoException) => error.code === 'ECONNREFUSED' ? resolve() : reject(error));
      });
    }
    try {
      const start = performance.now();
      for (let i = 0; i < 1_000; i++) {
        emitLog('warning', `pathb-outage-${i}`, { phase: 'outage' }, { logger: 'app.outage' });
      }
      // This exercises the real native enqueue path, not a mocked exporter.
      expect(performance.now() - start).toBeLessThan(1_000);
      const flushStart = performance.now();
      await expect(flushObservability(1_000)).rejects.toThrow();
      expect(performance.now() - flushStart).toBeLessThan(3_000);
      if (fault !== 'refused') expect(rejectedLogs).toBeGreaterThan(failedBefore);
      expect(got('pathb-outage-')).toBe(false);
      await assertReady();

      rejectLogs = false;
      releaseStalls();
      if (fault === 'refused') await startCollector(collectorPort);
      emitLog('warning', 'pathb-recovered', { phase: 'recovered' }, { logger: 'app.outage', traceparent: TP });
      await until(() => got('pathb-recovered'));
      const recovered = hits.find((hit) => hit.path.endsWith('/v1/logs') && hit.body.includes(Buffer.from('pathb-recovered')))!;
      expect(recovered.body.includes(Buffer.from('app.outage'))).toBe(true);
      expect(recovered.body.includes(Buffer.from('0af7651916cd43dd8448eb211c80319c', 'hex'))).toBe(true);
      pollLogs();
      expect(bridgedMessages.some((message) => message.includes('pathb-outage-') || message.includes('pathb-recovered'))).toBe(false);
      await assertReady();
      if (scope === 'readiness') return;

      rejectLogs = true;
      if (fault === 'refused') await closeCollector();
      emitLog('warning', 'pathb-outage-shutdown');
      const shutdownStart = performance.now();
      // Export failure must not prevent release of the pipeline reference.
      if (fault === 'stall') {
        await expect(shutdownObservability(1_000)).rejects.toThrow('continues in the background');
      } else {
        expect(await shutdownObservability(1_000)).toBe(true);
      }
      expect(performance.now() - shutdownStart).toBeLessThan(3_000);
      rejectLogs = false;
      releaseStalls();
      // A timed-out release consumed the native reference. Implicit startup
      // must open a new pipeline rather than trusting a stale JS ref count.
      await ensureObservability();
      expect(logStats().running).toBe(true);
      await shutdownObservability();
    } finally {
      rejectLogs = false;
      stallLogs = false;
      releaseStalls();
      await health?.stop();
      await node?.close();
      if (!collector.listening) await startCollector(collectorPort);
      await shutdownObservability();
      setLogSink(null);
    }
  }, 15_000);
});
