/**
 * Path B (design 3.4): the application's own log records go into the Rust OTel
 * logs SDK (`emitLog`, `AsterLogger({ otlp: true })`) and are exported over
 * OTLP/HTTP to a collector this file runs on 127.0.0.1.
 */

import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { gunzipSync } from 'node:zlib';
import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { initObservability, shutdownObservability, flushObservability, emitLog, AsterLogger, tracing, setLogSink, pollLogs } from '@aster-rpc/aster';

const hits: { path: string; body: Buffer }[] = [];
let collector: Server;
let rejectLogs = false;
let rejectedLogs = 0;

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

beforeAll(async () => {
  collector = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      let body = Buffer.concat(chunks);
      if (req.headers['content-encoding'] === 'gzip') body = gunzipSync(body);
      if (rejectLogs && req.url?.endsWith('/v1/logs')) {
        rejectedLogs++;
        res.writeHead(503);
        res.end();
        return;
      }
      hits.push({ path: req.url ?? '', body });
      res.writeHead(200, { 'content-type': 'application/x-protobuf' });
      res.end();
    });
  });
  await new Promise<void>((r) => collector.listen(0, '127.0.0.1', r));
  const endpoint = `http://127.0.0.1:${(collector.address() as AddressInfo).port}`;
  await initObservability({
    fromEnv: false,
    serviceName: 'pathb-test',
    otlpEndpoint: endpoint,
    exportOtlpLogs: true,
    exportTraces: true,
    logFilter: 'warn,aster=info',
  });
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

  it('keeps application emission bounded during exporter outage and exports after recovery', async () => {
    const failedBefore = rejectedLogs;
    const bridgedMessages: string[] = [];
    setLogSink((record) => bridgedMessages.push(record.message));
    rejectLogs = true;
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
      expect(rejectedLogs).toBeGreaterThan(failedBefore);
      expect(got('pathb-outage-')).toBe(false);

      rejectLogs = false;
      emitLog('warning', 'pathb-recovered', { phase: 'recovered' }, { logger: 'app.outage', traceparent: TP });
      await until(() => got('pathb-recovered'));
      const recovered = hits.find((hit) => hit.path.endsWith('/v1/logs') && hit.body.includes(Buffer.from('pathb-recovered')))!;
      expect(recovered.body.includes(Buffer.from('app.outage'))).toBe(true);
      expect(recovered.body.includes(Buffer.from('0af7651916cd43dd8448eb211c80319c', 'hex'))).toBe(true);
      pollLogs();
      expect(bridgedMessages.some((message) => message.includes('pathb-outage-') || message.includes('pathb-recovered'))).toBe(false);

      rejectLogs = true;
      emitLog('warning', 'pathb-outage-shutdown');
      const shutdownStart = performance.now();
      // Export failure must not prevent release of the pipeline reference.
      expect(await shutdownObservability(1_000)).toBe(true);
      expect(performance.now() - shutdownStart).toBeLessThan(3_000);
    } finally {
      rejectLogs = false;
      setLogSink(null);
    }
  }, 15_000);
});
