#!/usr/bin/env bun
/**
 * Mission Control benchmark — TypeScript client.
 *
 * Connects to a running Mission Control server (Python or TypeScript)
 * and measures throughput, latency percentiles, and memory usage.
 *
 * Usage:
 *     # Start a server (either language):
 *     python -m mission_control.server
 *     #   or
 *     bun run examples/typescript/missionControl/server.ts
 *
 *     # Run the benchmark:
 *     bun run examples/typescript/missionControl/benchmark.ts aster1...
 */

import { AsterClientWrapper } from '@aster-rpc/aster';
import { MissionControl } from './services.ts';

function memMB(): number {
  try {
    return process.memoryUsage.rss() / (1024 * 1024);
  } catch {
    return 0;
  }
}

function percentiles(latencies: number[]): { p50: number; p90: number; p99: number } {
  if (latencies.length === 0) return { p50: 0, p90: 0, p99: 0 };
  const s = [...latencies].sort((a, b) => a - b);
  const n = s.length;
  return {
    p50: s[Math.floor(n * 0.5)],
    p90: s[Math.floor(n * 0.9)],
    p99: s[Math.floor(n * 0.99)],
  };
}

function fmtLat(p: { p50: number; p90: number; p99: number }): string {
  return `p50=${p.p50.toFixed(2)}ms  p90=${p.p90.toFixed(2)}ms  p99=${p.p99.toFixed(2)}ms`;
}

const address = process.argv[2];
if (!address) {
  console.error("Usage: bun run benchmark.ts <server-address>");
  process.exit(1);
}

const memStart = memMB();
const client = new AsterClientWrapper({ address });
await client.connect();
const mc = client.proxy("MissionControl");

console.log(`Connected to ${address.slice(0, 30)}...`);
console.log(`Client memory at start: ${memStart.toFixed(1)} MB`);
console.log("─".repeat(72));
console.log("  ── Dynamic proxy client ────────────────────────────────────");

// ── Warmup ────────────────────────────────────────────────────────────────
for (let i = 0; i < 20; i++) {
  await mc.getStatus({ agent_id: "warmup" });
}

// ── Unary: getStatus ──────────────────────────────────────────────────────
const N_UNARY = 1000;
let lats: number[] = [];
let t0 = performance.now();
for (let i = 0; i < N_UNARY; i++) {
  const tc = performance.now();
  await mc.getStatus({ agent_id: `bench-${i}` });
  lats.push(performance.now() - tc);
}
let elapsed = (performance.now() - t0) / 1000;
let rps = N_UNARY / elapsed;
let p = percentiles(lats);
console.log(`  Unary (getStatus)    ${rps.toFixed(0).padStart(8)} req/s   ${fmtLat(p)}`);

// ── Unary: submitLog ──────────────────────────────────────────────────────
const N_LOG = 1000;
lats = [];
t0 = performance.now();
for (let i = 0; i < N_LOG; i++) {
  const tc = performance.now();
  await mc.submitLog({
    timestamp: Date.now() / 1000,
    level: "info",
    message: `bench log ${i}`,
    agent_id: "bench",
  });
  lats.push(performance.now() - tc);
}
elapsed = (performance.now() - t0) / 1000;
rps = N_LOG / elapsed;
p = percentiles(lats);
console.log(`  Unary (submitLog)    ${rps.toFixed(0).padStart(8)} req/s   ${fmtLat(p)}`);

// ── Client streaming: ingestMetrics ───────────────────────────────────────
for (const batchSize of [100, 1_000, 10_000]) {
  async function* metrics(n: number) {
    for (let i = 0; i < n; i++) {
      yield {
        name: "cpu.usage",
        value: 42.0 + (i % 100) * 0.1,
        timestamp: Date.now() / 1000,
      };
    }
  }

  t0 = performance.now();
  const result = await mc.ingestMetrics(metrics(batchSize)) as any;
  elapsed = (performance.now() - t0) / 1000;
  const mps = batchSize / elapsed;
  const accepted = result?.accepted ?? result;
  const label = batchSize.toLocaleString().padStart(5);
  console.log(`  Client stream (${label})  ${mps.toFixed(0).padStart(8)} msg/s   ${(elapsed * 1000).toFixed(1)}ms total   accepted=${accepted}`);
}

// ── Concurrent unary ──────────────────────────────────────────────────────
for (const concurrency of [10, 50, 100]) {
  t0 = performance.now();
  const tasks = Array.from({ length: concurrency }, (_, i) =>
    mc.getStatus({ agent_id: `concurrent-${i}` }),
  );
  await Promise.all(tasks);
  elapsed = (performance.now() - t0) / 1000;
  rps = concurrency / elapsed;
  const label = concurrency.toString().padStart(3);
  console.log(`  Concurrent (${label})      ${rps.toFixed(0).padStart(8)} req/s   ${(elapsed * 1000).toFixed(1)}ms total`);
}

// ── Typed client ──────────────────────────────────────────────────────────
console.log("  ── Typed client (generated from contract) ──────────────────");
const typed: any = await client.client(MissionControl as any);

for (let i = 0; i < 20; i++) {
  await typed.getStatus({ agent_id: "warmup" });
}

lats = [];
t0 = performance.now();
for (let i = 0; i < N_UNARY; i++) {
  const tc = performance.now();
  await typed.getStatus({ agent_id: `bench-${i}` });
  lats.push(performance.now() - tc);
}
elapsed = (performance.now() - t0) / 1000;
rps = N_UNARY / elapsed;
p = percentiles(lats);
console.log(`  Unary (getStatus)    ${rps.toFixed(0).padStart(8)} req/s   ${fmtLat(p)}`);

lats = [];
t0 = performance.now();
for (let i = 0; i < N_LOG; i++) {
  const tc = performance.now();
  await typed.submitLog({
    timestamp: Date.now() / 1000,
    level: "info",
    message: `bench log ${i}`,
    agent_id: "bench",
  });
  lats.push(performance.now() - tc);
}
elapsed = (performance.now() - t0) / 1000;
rps = N_LOG / elapsed;
p = percentiles(lats);
console.log(`  Unary (submitLog)    ${rps.toFixed(0).padStart(8)} req/s   ${fmtLat(p)}`);

for (const concurrency of [10, 50, 100]) {
  t0 = performance.now();
  const tasks = Array.from({ length: concurrency }, (_, i) =>
    typed.getStatus({ agent_id: `concurrent-${i}` }),
  );
  await Promise.all(tasks);
  elapsed = (performance.now() - t0) / 1000;
  rps = concurrency / elapsed;
  const label = concurrency.toString().padStart(3);
  console.log(`  Concurrent (${label})      ${rps.toFixed(0).padStart(8)} req/s   ${(elapsed * 1000).toFixed(1)}ms total`);
}

// ── Memory ────────────────────────────────────────────────────────────────
const memEnd = memMB();
console.log("─".repeat(72));
console.log(`  Memory: start=${memStart.toFixed(1)}MB  end=${memEnd.toFixed(1)}MB  delta=${(memEnd - memStart) >= 0 ? '+' : ''}${(memEnd - memStart).toFixed(1)}MB`);

await client.close();
