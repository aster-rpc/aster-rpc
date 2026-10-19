/**
 * R6 upcalls end to end: every JavaScript-implemented Rust extension point
 * driven by the real Rust adapter over the NAPI upcall sink — a lease
 * backend (in process and served as a designated grantor), lease
 * authorizers (async wrapper and the synchronous MemorySerializer hook), a
 * fenced resource, Gate 0, expose authorization and the topology `admitted`
 * filter — plus the mechanism itself: timeouts, fail-open / fail-closed,
 * cancellation (timeout, abandoned, shutdown), overload, out-of-order
 * answers, errors and unimplemented kinds. Offline: loopback only.
 */

import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { randomBytes } from 'node:crypto';
import {
  AdmittedFilter,
  FenceRejectedError,
  FencedOutcomeUnknownError,
  ForeignFencedResource,
  ForeignLeaseSerializer,
  MemorySerializer,
  Node,
  UpcallCancelledError,
  Upcalls,
  checkFence,
  leaseTransition,
  type Fence,
  type Lease,
  type LeaseBackend,
  type LeaseOp,
  type LeaseRowState,
  type LeaseSnapshot,
  type NodeId,
  type TransitionOutcome,
  type UpcallContext,
} from '@aster-rpc/aster';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;
// Fast renewals; a TTL that survives a stall on a loaded host.
const FAST = { ttlMs: 3000, renewIntervalMs: 150, holderMarginMs: 100 };

async function ready(node: Node): Promise<void> {
  for (let i = 0; i < 200 && node.directAddresses().length === 0; i++) {
    await new Promise((r) => setTimeout(r, 10));
  }
}

async function until<T>(what: string, f: () => Promise<T | undefined> | T | undefined, ms = 20_000): Promise<T> {
  const deadline = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v !== undefined && v !== false) return v;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 25));
  }
}

const never = <T>() => new Promise<T>(() => {});
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function granted(outcome: Awaited<ReturnType<ForeignLeaseSerializer['acquire']>>): Lease {
  if (!outcome.granted) throw new Error(`expected a grant, got ${outcome.reason}`);
  return outcome.lease;
}

/** A JavaScript lease store: rows in a Map, the normative state machine, this process's clock. */
class MapBackend implements LeaseBackend {
  readonly rows = new Map<string, { row: LeaseRowState; grantedAt?: number }>();
  readonly ops: Array<{ resource: string; op: LeaseOp }> = [];

  private get(resource: string) {
    return this.rows.get(resource) ?? { row: { epoch: 0, held: false, ttlMs: 0 } };
  }

  transition(resource: string, op: LeaseOp): TransitionOutcome {
    this.ops.push({ resource, op });
    const r = this.get(resource);
    const expired = r.row.held && (r.grantedAt == null || Date.now() - r.grantedAt >= r.row.ttlMs);
    const t = leaseTransition(r.row, expired, op, resource);
    if (t.kind === 'deny') return { kind: 'denied', denyReason: t.denyReason, snapshot: this.snap(resource) };
    this.rows.set(resource, { row: t.next, grantedAt: t.resetGrant ? Date.now() : r.grantedAt });
    return t.outcome;
  }

  async snapshot(resource: string): Promise<LeaseSnapshot> {
    return this.snap(resource);
  }

  snap(resource: string): LeaseSnapshot {
    const { row, grantedAt } = this.get(resource);
    return {
      resource,
      epoch: row.epoch,
      ...(row.holder != null ? { holder: row.holder } : {}),
      held: row.held,
      ...(row.held && grantedAt != null ? { remainingMs: Math.max(0, row.ttlMs - (Date.now() - grantedAt)) } : {}),
    };
  }
}

describe('lease backend in JavaScript', () => {
  it('acquire, deny, renew in the background, release — every transition answered by JS', async () => {
    const backend = new MapBackend();
    using ser = new ForeignLeaseSerializer(backend);
    const lease = granted(await ser.acquire('tree/42', 'n1', FAST));
    expect(lease).toMatchObject({ resource: 'tree/42', epoch: 1, holder: 'n1', status: 'held' });
    expect(backend.ops[0]).toEqual({ resource: 'tree/42', op: { kind: 'acquire', actor: 'n1', ttlMs: 3000 } });

    const denied = await ser.acquire('tree/42', 'n2', FAST);
    expect(denied.granted).toBe(false);
    if (!denied.granted) {
      expect(denied.reason).toBe('held_by_other');
      expect(denied.snapshot).toMatchObject({ resource: 'tree/42', epoch: 1, holder: 'n1', held: true });
    }

    // Renewals run in Rust and land in the JS backend.
    await sleep(3200); // > ttl
    expect(lease.status).toBe('held');
    expect(backend.ops.filter((o) => o.op.kind === 'renew').length).toBeGreaterThanOrEqual(2);
    expect(backend.ops.find((o) => o.op.kind === 'renew')?.op).toEqual({ kind: 'renew', actor: 'n1', epoch: 1 });

    expect(await ser.snapshot('tree/42')).toMatchObject({ epoch: 1, holder: 'n1', held: true });
    await lease.release();
    expect(backend.ops.at(-1)?.op).toEqual({ kind: 'release', actor: 'n1', epoch: 1 });
    expect(await ser.snapshot('tree/42')).toEqual({ resource: 'tree/42', epoch: 2, held: false });
    expect((await ser.revoke('tree/42', 'operator')).kind).toBe('revoked');
  });

  it('a backend error is an error (never a guessed outcome); a malformed answer too', async () => {
    const backend = new MapBackend();
    backend.snapshot = async () => {
      throw new Error('disk on fire');
    };
    backend.transition = () => ({ kind: 'bogus' }) as unknown as TransitionOutcome;
    using ser = new ForeignLeaseSerializer(backend);
    await expect(ser.snapshot('r')).rejects.toThrow(/disk on fire/);
    await expect(ser.acquire('r', 'n1', FAST)).rejects.toThrow(/invalid lease_transition answer/);
  });

  it('a kind without a handler is unimplemented', async () => {
    const upcalls = new Upcalls();
    const ser = new ForeignLeaseSerializer(new MapBackend(), { upcalls });
    ser.close(); // handlers removed; the shared table stays open
    await expect(ser.snapshot('r')).rejects.toThrow(/not implemented for lease_snapshot/);
    expect(() => new ForeignLeaseSerializer(new MapBackend(), { upcalls }).close()).not.toThrow();
    // One handler per kind per table.
    const first = new ForeignLeaseSerializer(new MapBackend(), { upcalls });
    expect(() => new ForeignLeaseSerializer(new MapBackend(), { upcalls })).toThrow(/already has a handler/);
    first.close();
    upcalls.close();
  });
});

describe('lease authorizers', () => {
  it('async authorizer around a JS backend: refusals are denied(unauthorized)', async () => {
    const asked: LeaseOp[] = [];
    using ser = new ForeignLeaseSerializer(new MapBackend(), {
      authorizer: async (op) => {
        asked.push(op);
        return op.actor !== 'mallory';
      },
    });
    const refused = await ser.acquire('r', 'mallory', FAST);
    expect(refused).toMatchObject({ granted: false, reason: 'unauthorized' });
    const lease = granted(await ser.acquire('r', 'alice', FAST));
    expect(asked.map((o) => o.actor)).toEqual(['mallory', 'alice']);
    await lease.release();
  });

  it('MemorySerializer.setAuthorizer (synchronous hook, answered by JS); null removes it', async () => {
    const ser = new MemorySerializer();
    ser.setAuthorizer({ authorize: (op) => op.kind === 'revoke' || op.actor === 'alice' });
    expect(await ser.acquire('r', 'bob', FAST)).toMatchObject({ granted: false, reason: 'unauthorized' });
    const lease = (await ser.acquire('r', 'alice', FAST)) as { granted: true; lease: Lease };
    expect(lease.granted).toBe(true);
    await sleep(400); // renewals ask too
    expect(lease.lease.status).toBe('held');
    await lease.lease.release();
    ser.setAuthorizer(null);
    const bob = await ser.acquire('r', 'bob', FAST);
    expect(bob.granted).toBe(true);
    if (bob.granted) await bob.lease.release();
  });

  it('timeouts apply the failure mode: fail-closed denies, fail-open allows; the handler is cancelled', async () => {
    const contexts: UpcallContext[] = [];
    const upcalls = new Upcalls({ policies: { lease_authorize: { timeoutMs: 150 } } });
    expect(upcalls.policy('lease_authorize')).toEqual({ timeoutMs: 150, failureMode: 'fail_closed' });
    using ser = new ForeignLeaseSerializer(new MapBackend(), {
      upcalls,
      authorizer: (_op, ctx) => {
        contexts.push(ctx);
        return never<boolean>();
      },
    });
    const t0 = Date.now();
    expect(await ser.acquire('r', 'alice', FAST)).toMatchObject({ granted: false, reason: 'unauthorized' });
    expect(Date.now() - t0).toBeLessThan(2000);
    await until('cancel delivered', () => contexts[0]?.signal.aborted);
    expect(contexts[0].signal.reason).toBeInstanceOf(UpcallCancelledError);
    expect((contexts[0].signal.reason as UpcallCancelledError).reason).toBe('timeout');
    expect(contexts[0].kind).toBe('lease_authorize');
    expect(contexts[0].deadline.getTime()).toBeGreaterThan(t0);

    upcalls.setPolicy('lease_authorize', { failureMode: 'fail_open' });
    const lease = granted(await ser.acquire('r', 'alice', { ...FAST, ttlMs: 5000, renewIntervalMs: 1000 }));
    lease.stopRenewing();
    expect(upcalls.pendingCount).toBe(0);
    upcalls.close();
  });

  it('close() cancels running handlers (shutdown) and fails pending upcalls', async () => {
    const contexts: UpcallContext[] = [];
    const upcalls = new Upcalls();
    const ser = new MemorySerializer();
    ser.setAuthorizer(
      (_op, ctx) => {
        contexts.push(ctx);
        return never<boolean>();
      },
      { upcalls },
    );
    const pending = ser.acquire('r', 'alice', FAST);
    await until('handler running', () => contexts.length === 1);
    expect(upcalls.pendingCount).toBe(1);
    upcalls.close();
    expect(upcalls.closed).toBe(true);
    // The decision falls back to fail-closed.
    expect(await pending).toMatchObject({ granted: false, reason: 'unauthorized' });
    expect((contexts[0].signal.reason as UpcallCancelledError).reason).toBe('shutdown');
    // Later upcalls fail at once, and a JS backend on the closed table errors.
    expect(await ser.acquire('r', 'alice', FAST)).toMatchObject({ granted: false, reason: 'unauthorized' });
    const backend = new ForeignLeaseSerializer(new MapBackend(), { upcalls });
    await expect(backend.snapshot('r')).rejects.toThrow(/closed/);
    backend.close();
  });
});

describe('fenced resource in JavaScript', () => {
  /** A JS resource: one row per resource, fence checked under "its lock". */
  function store() {
    const rows = new Map<string, LeaseRowState>();
    const data: string[] = [];
    const resource = {
      async mutate(fence: Fence, writer: string, op: Uint8Array) {
        const row = rows.get(fence.resource) ?? { epoch: 0, held: false, ttlMs: 0 };
        const rejection = checkFence(row, fence, writer);
        if (rejection) throw rejection;
        const text = Buffer.from(op).toString();
        if (text === 'boom') throw new Error('write failed');
        data.push(text);
        return Buffer.from(`ok:${text}`);
      },
    };
    return { rows, data, resource };
  }

  it('applies under the fence, rejects stale or foreign writers, unknown outcome on errors', async () => {
    const s = store();
    s.rows.set('acct', { epoch: 3, holder: 'w1', held: true, ttlMs: 1000 });
    using res = new ForeignFencedResource(s.resource);
    const fence = { resource: 'acct', epoch: 3, holder: 'w1' };
    expect(Buffer.from(await res.mutate(fence, 'w1', Buffer.from('a'))).toString()).toBe('ok:a');
    expect(s.data).toEqual(['a']);

    const stale = await res.mutate({ ...fence, epoch: 2 }, 'w1', Buffer.from('b')).catch((e) => e);
    expect(stale).toBeInstanceOf(FenceRejectedError);
    expect(stale).toMatchObject({ kind: 'stale_epoch', carried: 2, current: 3 });
    const foreign = await res.mutate(fence, 'w2', Buffer.from('c')).catch((e) => e);
    expect(foreign).toMatchObject({ kind: 'not_holder' });

    await expect(res.mutate(fence, 'w1', Buffer.from('boom'))).rejects.toBeInstanceOf(FencedOutcomeUnknownError);
    await expect(res.mutate(fence, 'w1', Buffer.from('boom'))).rejects.toThrow(/write failed/);
    expect(s.data).toEqual(['a']);
  });

  it('answers are matched by id: a slow mutation does not hold up a fast one', async () => {
    const order: string[] = [];
    using res = new ForeignFencedResource({
      async mutate(_f, _w, op) {
        const ms = op[0];
        await sleep(ms);
        order.push(String(ms));
        return Uint8Array.of(ms);
      },
    });
    const fence = { resource: 'x', epoch: 1, holder: 'w' };
    const slow = res.mutate(fence, 'w', Uint8Array.of(200));
    const fast = res.mutate(fence, 'w', Uint8Array.of(10));
    expect([...(await fast)]).toEqual([10]);
    expect([...(await slow)]).toEqual([200]);
    expect(order).toEqual(['10', '200']);
  });

  it('a timed-out mutation is unknown and its handler cancelled', async () => {
    const contexts: UpcallContext[] = [];
    const upcalls = new Upcalls({ defaultPolicy: { timeoutMs: 100 } });
    using res = new ForeignFencedResource(
      {
        mutate: (_f, _w, _op, ctx) => {
          contexts.push(ctx);
          return never<Uint8Array>();
        },
      },
      { upcalls },
    );
    const err = await res.mutate({ resource: 'x', epoch: 1, holder: 'w' }, 'w', Uint8Array.of(1)).catch((e) => e);
    expect(err).toBeInstanceOf(FencedOutcomeUnknownError);
    expect(err.message).toMatch(/timed out/);
    await until('cancel', () => contexts[0]?.signal.aborted);
    expect((contexts[0].signal.reason as UpcallCancelledError).reason).toBe('timeout');
    upcalls.close();
  });

  it('a full queue refuses upcalls at once (overload) while JS is busy', async () => {
    const upcalls = new Upcalls({ capacity: 1 });
    using res = new ForeignFencedResource({ mutate: (_f, _w, op) => op }, { upcalls });
    const fence = { resource: 'x', epoch: 1, holder: 'w' };
    const calls = [1, 2, 3].map((i) => res.mutate(fence, 'w', Uint8Array.of(i)).then(
      (v) => ({ ok: [...v] }),
      (e: Error) => ({ err: e.message }),
    ));
    // Keep the JS thread busy so nothing is drained while Rust delivers.
    const stop = Date.now() + 300;
    while (Date.now() < stop) {
      /* spin */
    }
    const results = await Promise.all(calls);
    const ok = results.filter((r) => 'ok' in r);
    const refused = results.filter((r) => 'err' in r && /refused/.test(r.err));
    expect(ok.length).toBe(1);
    expect(refused.length).toBe(2);
    expect(upcalls.stats().refused).toBe(2);
    upcalls.close();
  });
});

describe('lease grantor over a JavaScript backend', () => {
  let grantor: Node;
  let a: Node;
  let server: { close(): void };
  const backend = new MapBackend();
  let ser: ForeignLeaseSerializer;

  beforeAll(async () => {
    grantor = await Node.start({ ...OFFLINE, alpns: ['aster/1'] });
    a = await Node.start(OFFLINE);
    await Promise.all([ready(grantor), ready(a)]);
    a.native.addNodeAddr(grantor.native);
    ser = new ForeignLeaseSerializer(backend);
    server = grantor.serveLeaseGrantor(ser);
  });

  afterAll(async () => {
    server?.close();
    ser?.close();
    await Promise.all([grantor?.close(), a?.close()]);
  });

  it('remote acquires and renewals reach the JS backend bound to the caller', async () => {
    const remote = await a.leaseGrantor(grantor.id);
    const lease = granted(await remote.acquire('db/primary', FAST));
    expect(lease.holder).toBe(a.id);
    expect(backend.ops[0].op).toEqual({ kind: 'acquire', actor: a.id, ttlMs: 3000 });
    await sleep(600);
    expect(lease.status).toBe('held');
    expect(backend.ops.some((o) => o.op.kind === 'renew' && o.op.actor === a.id)).toBe(true);
    await lease.release();
    expect((await backend.snapshot('db/primary')).held).toBe(false);
  });
});

describe('Gate 0 answered by upcalls', () => {
  type Outcome = { closed: true; code?: number; reason?: string } | { closed: false };
  async function dial(client: Node, server: Node, alpn: string, ms = 1500): Promise<Outcome> {
    let conn;
    try {
      conn = await client.connect(server.addr, alpn);
    } catch (e) {
      return { closed: true, reason: String(e) };
    }
    const result = await Promise.race([
      conn.closed().then((c) => ({ closed: true as const, code: c.code ?? undefined, reason: c.reason ?? undefined })),
      new Promise<Outcome>((r) => setTimeout(() => r({ closed: false }), ms)),
    ]);
    if (!result.closed) conn.close(0, 'done');
    return result;
  }

  it('handshake hooks admit and reject; the iterator is then unavailable', async () => {
    await using server = await Node.start({ ...OFFLINE, alpns: ['r6/app'], enableHooks: true, hookTimeoutMs: 5000 });
    await using client = await Node.start(OFFLINE);
    await ready(server);
    const seen: Array<{ peer: string; alpn: string; isAlive: boolean }> = [];
    let n = 0;
    const hooks = server.serveAdmission({
      handshake: async (req) => {
        seen.push({ peer: req.peer, alpn: req.alpn, isAlive: req.isAlive });
        return n++ === 0 ? { accept: false, code: 499, reason: 'not today' } : true;
      },
    });
    expect(() => server.admission()).toThrow(/already being answered/);
    expect(await dial(client, server, 'r6/app')).toMatchObject({ closed: true, code: 499, reason: 'not today' });
    expect((await dial(client, server, 'r6/app', 500)).closed).toBe(false);
    expect(seen).toEqual([
      { peer: client.id, alpn: 'r6/app', isAlive: true },
      { peer: client.id, alpn: 'r6/app', isAlive: true },
    ]);
    // Hooks removed: unimplemented → fail-closed → 403 "hook unavailable".
    hooks.close();
    expect(await dial(client, server, 'r6/app')).toMatchObject({ closed: true, code: 403, reason: 'hook unavailable' });
  });

  it('the node giving up first cancels the upcall (abandoned)', async () => {
    await using server = await Node.start({ ...OFFLINE, alpns: ['r6/app'], enableHooks: true, hookTimeoutMs: 300 });
    await using client = await Node.start(OFFLINE);
    await ready(server);
    const contexts: UpcallContext[] = [];
    const hooks = server.serveAdmission({
      handshake: (_req, ctx) => {
        contexts.push(ctx);
        return never<boolean>();
      },
    });
    void dial(client, server, 'r6/app', 500);
    await until('abandoned', () => contexts[0]?.signal.aborted, 10_000);
    expect((contexts[0].signal.reason as UpcallCancelledError).reason).toBe('abandoned');
    hooks.close();
  });
});

describe('expose authorization in JavaScript', () => {
  let http: Server;
  let port: number;

  beforeAll(async () => {
    http = createServer((req, res) => res.end(`hi ${req.url}`));
    await new Promise<void>((r) => http.listen(0, '127.0.0.1', () => r()));
    port = (http.address() as AddressInfo).port;
  });

  afterAll(() => {
    http?.close();
  });

  it('exposes a socket target to the peers the authorizer admits', async () => {
    await using host = await Node.start({ ...OFFLINE, alpns: ['r6/expose'] });
    await using alice = await Node.start(OFFLINE);
    await using mallory = await Node.start(OFFLINE);
    await ready(host);
    alice.native.addNodeAddr(host.native);
    mallory.native.addNodeAddr(host.native);

    const asked: Array<{ serviceId: string; peer: string; metadata: string }> = [];
    using expose = host.expose();
    expose.exposeSocket('web', `127.0.0.1:${port}`, async ({ serviceId, peer, metadata }) => {
      asked.push({ serviceId, peer, metadata: Buffer.from(metadata).toString() });
      return peer === alice.id;
    });
    const server = host.serveExpose(expose, ['r6/expose']);
    const get = async (who: Node, metadata: string) => {
      const conn = await who.connect(host.addr, 'r6/expose');
      try {
        const bi = await conn.openExposeStream('web', Buffer.from(metadata));
        const send = bi.takeSend();
        const recv = bi.takeRecv();
        await send.writeAll(Buffer.from('GET /hello HTTP/1.1\r\nHost: web\r\nConnection: close\r\n\r\n'));
        await send.finish();
        return Buffer.from(await recv.readToEnd(1 << 20)).toString();
      } catch (e) {
        return `error: ${(e as Error).message}`;
      } finally {
        conn.close(0, 'done');
      }
    };
    try {
      const ok = await get(alice, 'alice-meta');
      expect(ok).toMatch(/^HTTP\/1\.1 200/);
      expect(ok).toMatch(/hi \/hello$/);
      const refused = await get(mallory, 'mallory-meta');
      expect(refused).not.toMatch(/200/);
      expect(asked).toEqual([
        { serviceId: 'web', peer: alice.id, metadata: 'alice-meta' },
        { serviceId: 'web', peer: mallory.id, metadata: 'mallory-meta' },
      ]);
    } finally {
      server.close();
    }
  });
});

describe('topology admitted filter in JavaScript', () => {
  const nodes: Node[] = [];

  beforeAll(async () => {
    for (let i = 0; i < 3; i++) {
      nodes.push(await Node.start({ ...OFFLINE, enableMonitoring: true }));
    }
    await Promise.all(nodes.map(ready));
    for (const x of nodes) for (const y of nodes) if (x !== y) x.native.addNodeAddr(y.native);
  });

  afterAll(async () => {
    await Promise.all(nodes.map((n) => n.close()));
  });

  it('filters producers at read time from cached, batched JS answers; set() applies at the next view', async () => {
    const ids = nodes.map((n) => n.id);
    const banned = new Set<string>([ids[2]]);
    const batches: NodeId[][] = [];
    using filter = new AdmittedFilter(async (batch) => {
      batches.push(batch);
      return batch.map((id) => !banned.has(id));
    });
    await filter.prefetch([ids[0], ids[1]]);
    expect(filter.isAdmitted(ids[0])).toBe(true);
    expect(filter.stats()).toMatchObject({ hits: 1, upcalls: 1 });

    const secret = randomBytes(32);
    const opts = (n: Node) => ({
      bootstrap: ids.filter((id) => id !== n.id),
      positionRefreshMs: 500,
      timing: { minHoldMs: 0, livenessTtlMs: 60_000 },
    });
    await nodes[0].topology().enableShared(secret, { ...opts(nodes[0]), admitted: filter });
    await nodes[1].topology().enableShared(secret, opts(nodes[1]));
    await nodes[2].topology().enableShared(secret, opts(nodes[2]));

    // The unfiltered view converges on all three; the filtered one leaves the banned producer out.
    await until(
      'unfiltered cluster of three',
      async () => ((await nodes[1].topology().myCluster())?.members.length ?? 0) === 3,
      30_000,
    );
    // Two members alone may predate the banned producer's first sighting:
    // wait until the filter was asked about it too.
    const filtered = await until(
      'filtered cluster of two after the banned producer was asked about',
      async () => {
        const c = await nodes[0].topology().myCluster();
        return c && c.members.length === 2 && batches.flat().includes(ids[2]) ? c : undefined;
      },
      30_000,
    );
    expect([...filtered.members]).toEqual([ids[0], ids[1]].sort());
    expect(batches.flat()).toContain(ids[2]);
    expect(filter.isAdmitted(ids[2])).toBe(false);
    const stats = filter.stats();
    expect(stats.upcalls).toBeGreaterThanOrEqual(2);
    expect(stats.hits).toBeGreaterThan(0);

    // Lift the ban: pushed answers apply at the next view.
    banned.delete(ids[2]);
    filter.set(ids[2], true);
    await until(
      'filtered view grows to three',
      async () => ((await nodes[0].topology().myCluster())?.members.length ?? 0) === 3,
      30_000,
    );
  }, 90_000);
});
