// R6 upcalls smoke on the *built* package (plain ESM): runs under Node and
// Bun (where vitest cannot run, plan §1.3). Exercises the NAPI upcall sink
// end to end — a JavaScript lease backend + authorizer, a fenced resource
// hammered sequentially and concurrently (the Bun TSFN stall watchdog's
// territory), and a timeout falling back to its failure mode — then checks
// the process can exit (the upcall TSFN is weak).
//
//   (cd bindings/typescript/packages/aster && npx tsc)
//   node tests/typescript/smoke/r6-upcalls-smoke.mjs
//   bun  tests/typescript/smoke/r6-upcalls-smoke.mjs

import { fileURLToPath } from 'node:url';
import * as path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const aster = await import(path.join(here, '../../../bindings/typescript/packages/aster/dist/index.js'));
const { ForeignFencedResource, ForeignLeaseSerializer, Upcalls, leaseTransition } = aster;

const runtime = typeof Bun !== 'undefined' ? `bun ${Bun.version}` : `node ${process.version}`;
const t0 = Date.now();
function check(cond, what) {
  if (!cond) throw new Error(`smoke failed: ${what}`);
}

// 1. A JavaScript lease backend behind a JavaScript authorizer.
const rows = new Map();
const backend = {
  transition(resource, op) {
    const r = rows.get(resource) ?? { row: { epoch: 0, held: false, ttlMs: 0 } };
    const expired = r.row.held && (r.at == null || Date.now() - r.at >= r.row.ttlMs);
    const t = leaseTransition(r.row, expired, op, resource);
    if (t.kind === 'deny') {
      return { kind: 'denied', denyReason: t.denyReason, snapshot: { resource, epoch: r.row.epoch, held: r.row.held } };
    }
    rows.set(resource, { row: t.next, at: t.resetGrant ? Date.now() : r.at });
    return t.outcome;
  },
  async snapshot(resource) {
    const r = rows.get(resource)?.row ?? { epoch: 0, held: false };
    return { resource, epoch: r.epoch, ...(r.holder ? { holder: r.holder } : {}), held: r.held };
  },
};
const ser = new ForeignLeaseSerializer(backend, { authorizer: async (op) => op.actor !== 'mallory' });
const refused = await ser.acquire('r', 'mallory', { ttlMs: 600, renewIntervalMs: 150, holderMarginMs: 100 });
check(!refused.granted && refused.reason === 'unauthorized', 'authorizer refusal');
const got = await ser.acquire('r', 'alice', { ttlMs: 600, renewIntervalMs: 150, holderMarginMs: 100 });
check(got.granted, 'grant');
await new Promise((r) => setTimeout(r, 700));
check(got.lease.status === 'held', 'renewed past the ttl through JS');
await got.lease.release();
check(!(await ser.snapshot('r')).held, 'released');
ser.close();

// 2. Many round trips: sequential, then concurrent.
const upcalls = new Upcalls();
const res = new ForeignFencedResource({ mutate: async (_f, _w, op) => op }, { upcalls });
const fence = { resource: 'x', epoch: 1, holder: 'w' };
for (let i = 0; i < 2000; i++) {
  const out = await res.mutate(fence, 'w', Uint8Array.of(i & 0xff));
  check(out[0] === (i & 0xff), `sequential echo ${i}`);
}
const many = await Promise.all(
  Array.from({ length: 500 }, (_, i) => res.mutate(fence, 'w', Uint8Array.of(i & 0xff))),
);
check(many.every((out, i) => out[0] === (i & 0xff)), 'concurrent echoes');
const stats = upcalls.stats();
res.close();
upcalls.close();

// 3. A hanging decision falls back to its failure mode after the timeout.
const timed = new Upcalls({ policies: { lease_authorize: { timeoutMs: 100, failureMode: 'fail_open' } } });
const hung = new ForeignLeaseSerializer(backend, { upcalls: timed, authorizer: () => new Promise(() => {}) });
const fallback = await hung.acquire('t', 'bob', { ttlMs: 5000, renewIntervalMs: 1000, holderMarginMs: 100 });
check(fallback.granted, 'fail-open fallback');
fallback.lease.stopRenewing();
hung.close();
timed.close();

console.log(
  `r6 upcalls smoke ok on ${runtime} in ${Date.now() - t0} ms ` +
    `(upcalls ${stats.delivered}, tsfn calls ${stats.tsfnCalls}, watchdog rescues ${stats.watchdogRescues})`,
);
