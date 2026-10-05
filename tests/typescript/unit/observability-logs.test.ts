/**
 * O7: the Rust log bridge delivered to JavaScript (design §3.2).
 *
 * - N Rust records reach the sink (weak TSFN, in order, with fields);
 * - a full TSFN queue counts drops (the JS thread blocked while Rust emits);
 * - the `pollLogs` fallback (what Bun uses) delivers the same records;
 * - the bridge filter is reloadable;
 * - the pino and winston adapters.
 *
 * The pipeline is once per process, and vitest runs each file in its own
 * worker, so this file initialises it once for all its tests. Records come from
 * the addon's `emitTestLogs` hook (target `aster::o7test`).
 */

import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import {
  initObservability,
  shutdownObservability,
  setLogSink,
  setLogFilter,
  setLogDelivery,
  pollLogs,
  logStats,
  pinoSink,
  winstonSink,
  type RustLogRecord,
} from '@aster-rpc/aster';
import { loadNative } from '../../../bindings/typescript/packages/aster/src/native.ts';

const native = loadNative();
const FILTER = 'off,aster::o7test=trace';

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function until(cond: () => boolean, ms = 10_000): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error(`timed out waiting (stats ${JSON.stringify(logStats())})`);
    await sleep(5);
  }
}

/** A sink that records only this test's records. */
function collector(): { records: RustLogRecord[] } {
  const c = { records: [] as RustLogRecord[] };
  setLogSink((r) => {
    if (r.target === 'aster::o7test') c.records.push(r);
  });
  return c;
}

async function emitted(n: number, base: number): Promise<void> {
  await until(() => logStats().testEmitted >= base + n);
}

beforeAll(async () => {
  await initObservability({ logFilter: FILTER, logQueue: 8192 });
});

afterAll(async () => {
  setLogSink(null);
  await shutdownObservability();
});

describe('Rust log records reach the sink', () => {
  it('delivers N records in order, with level, logger and fields', async () => {
    const c = collector();
    const base = logStats().testEmitted;
    native.emitTestLogs(200, 0, 3, 'rec');
    await until(() => c.records.length >= 200);
    expect(c.records).toHaveLength(200);
    c.records.forEach((r, i) => {
      expect(r.message).toBe(`rec ${i}`);
      expect(r.level).toBe(3);
      expect(r.logger).toBe('aster.o7test');
      expect(r.fields.idx).toBe(String(i));
      expect(r.timestampMs).toBeGreaterThan(Date.now() - 60_000);
    });
    await emitted(200, base);
    const s = logStats();
    expect(s.running).toBe(true);
    expect(s.pollMode).toBe(false);
    expect(s.sinkDropped).toBe(0);
    expect(s.bridgeDropped).toBe(0);
  });

  it('the bridge filter is reloadable', async () => {
    const c = collector();
    setLogFilter('off,aster::o7test=warn');
    try {
      const base = logStats().testEmitted;
      native.emitTestLogs(5, 0, 3, 'quiet'); // info: filtered in Rust
      native.emitTestLogs(5, 0, 2, 'loud'); // warn
      await emitted(10, base);
      await until(() => c.records.length >= 5);
      await sleep(100);
      expect(c.records.map((r) => r.message)).toEqual([0, 1, 2, 3, 4].map((i) => `loud ${i}`));
    } finally {
      setLogFilter(FILTER);
    }
  });
});

describe('a full TSFN queue counts drops', () => {
  it('blocks the JS thread while Rust emits; every record is delivered or counted', async () => {
    // One record per batch and no coalescing make the 64-batch TSFN bound
    // reachable at once.
    setLogDelivery({ maxBatch: 1, holdAbove: 64 });
    const c = collector();
    const before = logStats();
    const base = before.testEmitted;
    const N = 600;
    native.emitTestLogs(N, 0, 3, 'flood');
    // Starve the event loop: no TSFN callback can run for 400 ms.
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 400);
    await emitted(N, base);
    await until(() => {
      const s = logStats();
      return s.pending === 0 && s.forwarded - before.forwarded + (s.sinkDropped - before.sinkDropped) >= N
        && s.delivered - before.delivered >= s.forwarded - before.forwarded;
    });
    const s = logStats();
    const dropped = s.sinkDropped - before.sinkDropped;
    expect(dropped).toBeGreaterThan(0);
    expect(s.droppedBatches).toBeGreaterThan(0);
    // Nothing vanishes: delivered + dropped by the TSFN + dropped by Rust = emitted.
    expect(c.records.length + dropped + (s.bridgeDropped - before.bridgeDropped)).toBe(N);
    // The bound is 64 queued batches (one record each), plus what ran meanwhile.
    expect(c.records.length).toBeLessThan(N);
    // Survivors are still in order.
    const idx = c.records.map((r) => Number(r.fields.idx));
    expect([...idx].sort((a, b) => a - b)).toEqual(idx);
    setLogDelivery({});
  });
});

describe('bursts are coalesced, not dropped', () => {
  it('a starved JS thread still gets every record (default delivery)', async () => {
    const c = collector();
    const before = logStats();
    const base = before.testEmitted;
    const N = 600;
    native.emitTestLogs(N, 0, 3, 'burst');
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 400);
    await emitted(N, base);
    await until(() => c.records.length >= N);
    const s = logStats();
    expect(c.records.map((r) => r.message)).toEqual(Array.from({ length: N }, (_, i) => `burst ${i}`));
    expect(s.sinkDropped - before.sinkDropped).toBe(0);
    expect(s.bridgeDropped - before.bridgeDropped).toBe(0);
  });
});

describe('pollLogs fallback (the Bun path)', () => {
  it('delivers N records with the timer and with an explicit pollLogs()', async () => {
    setLogDelivery({ poll: true, pollIntervalMs: 20 });
    try {
      expect(logStats().pollMode).toBe(true);
      const c = collector();
      const before = logStats();
      native.emitTestLogs(120, 0, 3, 'polled');
      await until(() => c.records.length >= 120);
      expect(c.records.map((r) => r.message)).toEqual(Array.from({ length: 120 }, (_, i) => `polled ${i}`));
      const s = logStats();
      expect(s.polled - before.polled).toBe(120);
      // No TSFN delivery happened in poll mode.
      expect(s.forwarded).toBe(before.forwarded);

      // And on demand, without waiting for the timer.
      const base = s.testEmitted;
      native.emitTestLogs(10, 0, 3, 'now');
      await until(() => logStats().testEmitted >= base + 10);
      await until(() => logStats().pending >= 10 || c.records.length >= 130);
      pollLogs();
      expect(c.records).toHaveLength(130);
    } finally {
      setLogDelivery({});
    }
  });
});

describe('adapters', () => {
  it('pinoSink maps levels and fields', async () => {
    const seen: Array<[string, Record<string, unknown>, string]> = [];
    const mk = (lvl: string) => (o: Record<string, unknown>, m: string) => {
      if (o.logger === 'aster.o7test') seen.push([lvl, o, m]);
    };
    setLogSink(pinoSink({ error: mk('error'), warn: mk('warn'), info: mk('info'), debug: mk('debug'), trace: mk('trace') }));
    native.emitTestLogs(1, 0, 2, 'to-pino');
    native.emitTestLogs(1, 0, 5, 'to-pino-trace');
    await until(() => seen.length >= 2);
    const warn = seen.find(([, , m]) => m === 'to-pino 0')!;
    expect(warn[0]).toBe('warn');
    expect(warn[1].aster_origin).toBe('rust');
    expect(warn[1].idx).toBe('0');
    expect(seen.find(([, , m]) => m === 'to-pino-trace 0')![0]).toBe('trace');
  });

  it('winstonSink maps trace to silly', async () => {
    const seen: Array<Record<string, unknown>> = [];
    setLogSink(winstonSink({ log: (e) => { if (e.logger === 'aster.o7test') seen.push(e); } }));
    native.emitTestLogs(1, 0, 5, 'to-winston');
    native.emitTestLogs(1, 0, 1, 'to-winston-err');
    await until(() => seen.length >= 2);
    expect(seen.find((e) => e.message === 'to-winston 0')!.level).toBe('silly');
    expect(seen.find((e) => e.message === 'to-winston-err 0')!.level).toBe('error');
  });
});
