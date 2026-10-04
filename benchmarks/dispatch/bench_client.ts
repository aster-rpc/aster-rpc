#!/usr/bin/env bun
/**
 * Dispatch benchmark -- TypeScript own-language client (`benchmarks/dispatch/`).
 *
 * Drives an `InteropEcho` server (`tests/interop/`) with the generated TS
 * client over one connection and prints one `RESULT {json}` line per
 * measurement, in the same schema as `driver.rs`. Orchestrated by
 * `benchmarks/dispatch/run.py`. Needs `tests/interop/ts/aster-rpc.generated.ts`
 * (written by `tests/interop/run.py` / `run.py --build`).
 *
 *   bun run benchmarks/dispatch/bench_client.ts <aster1-ticket> [--duration S] [--warmup S]
 *       [--concurrency 1,10,50,100] [--stream-frames 100,10000] [--client-stream 10000] [--skip-streams]
 *
 * Unary goes through the binding's pooled fast path (`IrohTransport.unary` ->
 * `AsterCall.unaryFastPath`). Request: EchoRequest{message:"bench", count:1, data: 64 x 0xab}.
 */

import {
  IrohTransport,
  createClient,
  createXlangCodec,
  getXlangForyAndType,
  registerGenerated,
  i32,
} from '@aster-rpc/aster';
import { loadNative } from '@aster-rpc/aster/native.js';
import { SERVICES, WIRE_TYPES, BUILD_ALL_TYPES } from '../../tests/interop/ts/aster-rpc.generated.ts';
import { InteropEcho } from '../../tests/interop/ts/src/service.ts';
import { EchoRequest, EchoReply } from '../../tests/interop/ts/src/types.ts';

const RPC_ALPN = Buffer.from('aster/1');

function benchRequest(): EchoRequest {
  return new EchoRequest({ message: 'bench', count: i32(1), data: new Uint8Array(64).fill(0xab) });
}

function streamRequest(frames: number): EchoRequest {
  return new EchoRequest({ message: 's', count: i32(frames), data: new Uint8Array(64).fill(0xcd) });
}

function arg(name: string, dflt: string): string {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] !== undefined ? process.argv[i + 1] : dflt;
}
const ints = (s: string) => s.split(',').filter(Boolean).map(Number);

function pct(sorted: number[], p: number): number {
  if (!sorted.length) return 0;
  return sorted[Math.min(Math.floor(sorted.length * p), sorted.length - 1)] / 1e6;
}

function median(v: number[]): number {
  const s = [...v].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)];
}

const round = (x: number, d: number) => Math.round(x * 10 ** d) / 10 ** d;

async function runUnary(client: any, conc: number, durationS: number) {
  let stop = false;
  let errors = 0;
  let firstErr: string | undefined;
  const req = benchRequest();
  const buckets: number[][] = Array.from({ length: conc }, () => []);
  const worker = async (lats: number[]) => {
    while (!stop) {
      const t = process.hrtime.bigint();
      try {
        const reply = (await client.echo(req)) as EchoReply;
        if (reply.count !== 2) throw new Error(`bad reply count ${reply.count}`);
        lats.push(Number(process.hrtime.bigint() - t));
      } catch (e) {
        errors++;
        firstErr ??= String(e);
      }
    }
  };
  const start = performance.now();
  const tasks = buckets.map(worker);
  await new Promise((r) => setTimeout(r, durationS * 1000));
  stop = true;
  await Promise.all(tasks);
  const elapsed = (performance.now() - start) / 1000;
  if (firstErr) console.error(`worker error: ${firstErr}`);
  const lats = buckets.flat().sort((a, b) => a - b);
  return { n: lats.length, errors, elapsed, lats };
}

async function main(): Promise<number> {
  const ticket = process.argv[2];
  if (!ticket || ticket.startsWith('--')) {
    console.error('usage: bench_client.ts <aster1-ticket> [options]');
    return 2;
  }
  const duration = Number(arg('--duration', '5'));
  const warmup = Number(arg('--warmup', '1'));
  const concurrency = ints(arg('--concurrency', '1,10,50,100'));
  const streamFrames = ints(arg('--stream-frames', '100,10000'));
  const clientStream = ints(arg('--client-stream', '10000'));
  const skipStreams = process.argv.includes('--skip-streams');

  const native = loadNative();
  const { fory, Type } = getXlangForyAndType();
  const codec = createXlangCodec(fory, Type);
  registerGenerated({ SERVICES, WIRE_TYPES, buildAllTypes: BUILD_ALL_TYPES, codec, fory, Type } as any);
  const node = await native.Node.start({ alpns: [RPC_ALPN], relayMode: 'disabled', bindAddr: '127.0.0.1:0' });
  const t = native.asterTicketFromString(ticket);
  const conn = await node.connectNodeAddr(t.endpointId, RPC_ALPN, t.directAddrs, undefined);
  const client: any = createClient(InteropEcho, new IrohTransport(conn, codec));

  const first = (await client.echo(benchRequest())) as EchoReply;
  if (first.count !== 2) throw new Error(`bad first reply ${JSON.stringify(first)}`);
  if (warmup > 0) await runUnary(client, 10, warmup);

  for (const conc of concurrency) {
    if (warmup > 0) await runUnary(client, conc, Math.min(warmup, 1));
    const { n, errors, elapsed, lats } = await runUnary(client, conc, duration);
    console.log('RESULT ' + JSON.stringify({
      kind: 'unary', mode: 'ts', conc, calls: n, errors, secs: round(elapsed, 3),
      rps: round(n / elapsed, 1), p50_ms: round(pct(lats, 0.5), 3),
      p90_ms: round(pct(lats, 0.9), 3), p99_ms: round(pct(lats, 0.99), 3),
    }));
  }

  if (!skipStreams) {
    for (const frames of streamFrames) {
      const rates: number[] = [];
      const budget = performance.now();
      while (rates.length < 3 || (performance.now() - budget < 2000 && rates.length < 200)) {
        const t0 = performance.now();
        let got = 0;
        for await (const _ of client.generate(streamRequest(frames)) as AsyncIterable<EchoReply>) got++;
        if (got !== frames) throw new Error(`server-stream got ${got} != ${frames}`);
        rates.push(frames / ((performance.now() - t0) / 1000));
      }
      console.log('RESULT ' + JSON.stringify({
        kind: 'server_stream', frames, iterations: rates.length, frames_per_s: round(median(rates), 1),
      }));
    }
    for (const n of clientStream) {
      const rates: number[] = [];
      for (let k = 0; k < 3; k++) {
        const reqs = Array.from({ length: n }, () => streamRequest(1));
        async function* gen() {
          for (const r of reqs) yield r;
        }
        const t0 = performance.now();
        const reply = (await client.ingest(gen())) as EchoReply;
        if (reply.count !== n) throw new Error(`client-stream count ${reply.count} != ${n}`);
        rates.push(n / ((performance.now() - t0) / 1000));
      }
      console.log('RESULT ' + JSON.stringify({
        kind: 'client_stream', messages: n, iterations: 3, msgs_per_s: round(median(rates), 1),
      }));
    }
  }
  try { conn.close(0, 'done'); } catch { /* ignore */ }
  await node.close();
  return 0;
}

process.exit(await main());
