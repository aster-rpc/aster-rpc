/**
 * R6 upcalls through the NAPI sink: that each JavaScript extension point (a
 * lease backend, lease authorizers, a fenced resource, Gate 0, expose
 * authorization) is wired to the Rust adapter, that answers and errors map
 * both ways, and that a timeout cancel reaches the handler. The mechanism
 * itself (overload, out-of-order answers, shutdown and abandoned cancels,
 * the topology filter's caching) is tested once in Rust:
 * aster/tests/it/upcalls.rs and ffi/tests/upcalls.rs. Offline: loopback only.
 */

import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import {
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

    // Renewals run in Rust and land in the JS backend (renewal timing and
    // past-TTL holding: ffi/tests/upcalls.rs foreign_serializer_acquire_renew_fence_release).
    const renew = await until('a renewal answered by JS', () => backend.ops.find((o) => o.op.kind === 'renew'));
    expect(renew.op).toEqual({ kind: 'renew', actor: 'n1', epoch: 1 });

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
  it('MemorySerializer.setAuthorizer (synchronous hook, answered by JS); null removes it', async () => {
    const ser = new MemorySerializer();
    ser.setAuthorizer({ authorize: (op) => op.kind === 'revoke' || op.actor === 'alice' });
    expect(await ser.acquire('r', 'bob', FAST)).toMatchObject({ granted: false, reason: 'unauthorized' });
    const lease = (await ser.acquire('r', 'alice', FAST)) as { granted: true; lease: Lease };
    expect(lease.granted).toBe(true);
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

