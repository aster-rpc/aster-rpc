// O7 observability smoke on the *built* package (plain ESM): runs under Node
// and Bun (where vitest cannot run, plan §1.3). Checks, in a real process:
//
//   1. N Rust log records actually arrive in a JavaScript sink. Under Bun
//      delivery is the poll-timer fallback (the native TSFN can stall there);
//      under Node it is the weak TSFN.
//   2. A span that is still batched is exported by the `beforeExit` flush when
//      the process simply runs out of work (no explicit shutdown, no
//      process.exit before the flush).
//
//   (cd bindings/typescript/packages/aster && npx tsc)
//   node tests/typescript/smoke/o7-observability-smoke.mjs
//   bun  tests/typescript/smoke/o7-observability-smoke.mjs

import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const aster = await import(path.join(here, '../../../bindings/typescript/packages/aster/dist/index.js'));
const native = createRequire(import.meta.url)(path.join(here, '../../../bindings/typescript/native/index.js'));
const { initObservability, setLogSink, logStats, tracing } = aster;

const onBun = typeof Bun !== 'undefined';
const runtime = onBun ? `bun ${Bun.version}` : `node ${process.version}`;
const N = 50;
const SPAN = 'o7.smoke.beforeexit';
const t0 = Date.now();
let flushed = false;

function fail(msg) {
  console.error(`o7 smoke failed on ${runtime}: ${msg}`);
  process.exit(1);
}

// A collector that does not keep the process alive.
const collector = createServer((req, res) => {
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', () => {
    res.writeHead(200).end();
    if (req.url === '/v1/traces' && Buffer.concat(chunks).includes(Buffer.from(SPAN))) {
      flushed = true;
      console.log(
        `o7 smoke ok on ${runtime} in ${Date.now() - t0} ms ` +
          `(${N} records, poll mode ${stats.pollMode}, tsfn callbacks ${stats.callbacks}, ` +
          `polled ${stats.polled}, flushed on beforeExit)`,
      );
      process.exit(0);
    }
  });
});
await new Promise((r) => collector.listen(0, '127.0.0.1', r));
collector.unref();

await initObservability({
  fromEnv: false,
  serviceName: 'o7-smoke',
  otlpEndpoint: `http://127.0.0.1:${collector.address().port}`,
  exportTraces: true,
  logFilter: 'off,aster::o7test=trace',
});

// 1. Rust records reach the sink.
const got = [];
setLogSink((r) => {
  if (r.target === 'aster::o7test') got.push(r);
});
native.emitTestLogs(N, 200, 3, 'smoke');
await new Promise((resolve) => {
  const deadline = Date.now() + 15_000;
  const t = setInterval(() => {
    if (got.length >= N || Date.now() > deadline) {
      clearInterval(t);
      resolve();
    }
  }, 20);
});
if (got.length !== N) fail(`${got.length}/${N} records arrived (${JSON.stringify(logStats())})`);
got.forEach((r, i) => {
  if (r.message !== `smoke ${i}`) fail(`record ${i} out of order: ${r.message}`);
});
const stats = logStats();
if (stats.sinkDropped !== 0) fail(`dropped ${stats.sinkDropped} records`);
if (onBun && !stats.pollMode) fail('Bun should be polling');
if (onBun && stats.polled !== N) fail(`Bun delivered ${stats.polled} records by polling, not ${N}`);

// 2. Nothing else keeps the loop alive: the export can only come from beforeExit.
tracing.withSpan(SPAN, () => {});
process.on('exit', () => {
  if (!flushed) {
    console.error(`o7 smoke failed on ${runtime}: the span was not flushed on beforeExit`);
    process.exitCode = 1;
  }
});
