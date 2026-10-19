/**
 * Plan T7 behaviour on live nodes: fenced leases (in-memory serializer and a
 * designated grantor over RPC), pinned-author policy docs carrying sealed
 * grants, and topology v1 / v2 across three local nodes. Offline: loopback
 * only, relays disabled.
 */

import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { randomBytes } from 'node:crypto';
import {
  FenceRejectedError,
  GrantRecord,
  MemorySerializer,
  NamespaceCapability,
  Node,
  PolicyDoc,
  grantKey,
  reconcileNamespaceGrant,
  sealNamespaceGrant,
  type GrantContext,
  type Lease,
  type LeaseStatus,
} from '@aster-rpc/aster';
import { WAIT_BUDGET_MS } from '../helpers/wait-budget.ts';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;
// Fast renewals; a TTL that survives a stall on a loaded host.
const FAST = { ttlMs: 3000, renewIntervalMs: 150, holderMarginMs: 100 };

async function ready(node: Node): Promise<void> {
  for (let i = 0; i < 200 && node.directAddresses().length === 0; i++) {
    await new Promise((r) => setTimeout(r, 10));
  }
}

async function within<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let t: ReturnType<typeof setTimeout>;
  return Promise.race([
    p.finally(() => clearTimeout(t)),
    new Promise<T>((_, rej) => {
      t = setTimeout(() => rej(new Error(`timed out: ${what}`)), ms);
    }),
  ]);
}

async function until<T>(what: string, f: () => Promise<T | undefined> | T | undefined, ms = 20_000): Promise<T> {
  const deadline = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v !== undefined) return v;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 200));
  }
}

function granted(outcome: Awaited<ReturnType<MemorySerializer['acquire']>>): Lease {
  if (!outcome.granted) throw new Error(`expected a grant, got ${outcome.reason}`);
  return outcome.lease;
}

describe('lease: in-memory serializer', () => {
  it('acquire, deny, fenced mutation, release, re-acquire', async () => {
    const ser = new MemorySerializer();
    const lease = granted(await ser.acquire('tree/42', 'n1', FAST));
    expect(lease).toMatchObject({ resource: 'tree/42', epoch: 1, holder: 'n1', status: 'held' });
    const fence = lease.fence()!;
    expect(fence).toEqual({ resource: 'tree/42', epoch: 1, holder: 'n1' });

    const denied = await ser.acquire('tree/42', 'n2', FAST);
    expect(denied.granted).toBe(false);
    if (!denied.granted) {
      expect(denied.reason).toBe('held_by_other');
      expect(denied.snapshot).toMatchObject({ resource: 'tree/42', epoch: 1, holder: 'n1', held: true });
      expect(denied.snapshot.remainingMs).toBeGreaterThan(0);
    }

    expect(ser.mutateFenced(fence, 'n1', () => 'written')).toBe('written');
    // The fence is not a bearer token: another writer is refused.
    expect(() => ser.mutateFenced(fence, 'n2', () => 'nope')).toThrow(FenceRejectedError);

    await lease.release();
    expect(lease.status).toBe('released');
    expect(lease.fence()).toBeNull();
    await lease.release(); // idempotent

    let ran = false;
    try {
      ser.mutateFenced(fence, 'n1', () => {
        ran = true;
      });
      expect.unreachable();
    } catch (err) {
      expect(err).toBeInstanceOf(FenceRejectedError);
      expect(err).toMatchObject({ kind: 'stale_epoch', carried: 1, current: 2 });
    }
    expect(ran).toBe(false);

    const next = granted(await ser.acquire('tree/42', 'n2', FAST));
    expect(next.epoch).toBe(3);
    await next.release();
    expect(await ser.snapshot('tree/42')).toEqual({ resource: 'tree/42', epoch: 4, held: false });
  });

  it('renews past the TTL in the background', async () => {
    const ser = new MemorySerializer();
    const lease = granted(await ser.acquire('r', 'n1', FAST));
    await new Promise((r) => setTimeout(r, 3200)); // > ttl
    expect(lease.status).toBe('held');
    expect(lease.fence()?.epoch).toBe(1);
    expect((await ser.acquire('r', 'n2', FAST)).granted).toBe(false);
    await lease.release();
  });

  it('a revoke fences the holder; statusChanges reports it and ends', async () => {
    const ser = new MemorySerializer();
    const lease = granted(await ser.acquire('r', 'n1', FAST));
    const seen: LeaseStatus[] = [];
    const watching = (async () => {
      for await (const s of lease.statusChanges()) seen.push(s);
    })();
    expect((await ser.revoke('r', 'operator')).kind).toBe('revoked');
    await within(watching, WAIT_BUDGET_MS, 'fenced status');
    expect(seen).toEqual(['fenced']);
    expect(lease.status).toBe('fenced');
    expect(lease.fence()).toBeNull();
    // Already terminal: a new iterator yields the status and ends.
    const again: LeaseStatus[] = [];
    for await (const s of lease.statusChanges()) again.push(s);
    expect(again).toEqual(['fenced']);
  });

  it('await using releases on scope exit; stopRenewing keeps the row held', async () => {
    const ser = new MemorySerializer();
    {
      await using lease = granted(await ser.acquire('scoped', 'n1', FAST));
      expect(lease.fence()).not.toBeNull();
    }
    expect((await ser.snapshot('scoped')).held).toBe(false);

    const lease = granted(await ser.acquire('final', 'n1', FAST));
    const fence = lease.stopRenewing();
    expect(fence).toEqual({ resource: 'final', epoch: 1, holder: 'n1' });
    expect(lease.status).toBe('released');
    expect((await ser.snapshot('final')).held).toBe(true);
    expect(ser.mutateFenced(fence!, 'n1', () => 1)).toBe(1);
  });

  it('validates options', async () => {
    const ser = new MemorySerializer();
    await expect(ser.acquire('r', 'n1', { ttlMs: 1000, renewIntervalMs: 950, holderMarginMs: 100 })).rejects.toThrow(
      /renew_interval/,
    );
    await expect(ser.acquire('r', 'n1', { ttlMs: -1 })).rejects.toThrow(/ttlMs/);
  });
});

describe('lease: designated grantor over RPC', () => {
  let grantor: Node;
  let a: Node;
  let b: Node;
  let store: MemorySerializer;
  let server: { close(): void };

  beforeAll(async () => {
    grantor = await Node.start({ ...OFFLINE, alpns: ['aster/1'] });
    [a, b] = await Promise.all([Node.start(OFFLINE), Node.start(OFFLINE)]);
    await Promise.all([ready(grantor), ready(a), ready(b)]);
    a.native.addNodeAddr(grantor.native);
    b.native.addNodeAddr(grantor.native);
    store = new MemorySerializer();
    server = grantor.serveLeaseGrantor(store);
  });

  afterAll(async () => {
    server?.close();
    await Promise.all([grantor?.close(), a?.close(), b?.close()]);
  });

  it('binds grants to the authenticated caller and hands off on release', async () => {
    const sa = await within(a.leaseGrantor(grantor.id), 10_000, 'connect a');
    const sb = await within(b.leaseGrantor(grantor.id), 10_000, 'connect b');
    expect(sa.selfId).toBe(a.id);

    const outcome = await sa.acquire('db/primary', FAST);
    expect(outcome.granted).toBe(true);
    if (!outcome.granted) return;
    const lease = outcome.lease;
    expect(lease.holder).toBe(a.id);
    expect(lease.fence()).toEqual({ resource: 'db/primary', epoch: 1, holder: a.id });

    // The grantor-side store is the co-located enforcement point.
    expect(store.mutateFenced(lease.fence()!, a.id, () => 'ok')).toBe('ok');

    const denied = await sb.acquire('db/primary', FAST);
    expect(denied.granted).toBe(false);
    if (!denied.granted) {
      expect(denied.reason).toBe('held_by_other');
      expect(denied.snapshot.holder).toBe(a.id);
    }

    // Renewals over RPC keep it held past the TTL.
    await new Promise((r) => setTimeout(r, 3200));
    expect(lease.status).toBe('held');
    expect(await sb.snapshot('db/primary')).toMatchObject({ epoch: 1, holder: a.id, held: true });

    // revoke is operator-gated by default.
    await expect(sa.revoke('db/primary')).rejects.toThrow();

    await lease.release();
    const next = await sb.acquire('db/primary', FAST);
    expect(next.granted && next.lease.epoch).toBe(3);
    if (next.granted) await next.lease.release();
  });
});

describe('policy doc + sealed grants', () => {
  let root: Node;
  let member: Node;

  beforeAll(async () => {
    [root, member] = await Promise.all([Node.start(OFFLINE), Node.start(OFFLINE)]);
    await Promise.all([ready(root), ready(member)]);
    root.native.addNodeAddr(member.native);
    member.native.addNodeAddr(root.native);
  });

  afterAll(async () => {
    await Promise.all([root?.close(), member?.close()]);
  });

  it('pins the root author and delivers a namespace grant across nodes', async () => {
    const rootDocs = root.docs();
    const rootAuthor = await rootDocs.defaultAuthor();
    const forger = await rootDocs.createAuthor();
    const doc = await rootDocs.create();
    const nsId = doc.docId();

    const topoSecret = randomBytes(32);
    const ctx: GrantContext = {
      app: 'ts/topo-grant/v1',
      granter: root.id,
      recipient: member.id,
      resource: Buffer.from(nsId, 'hex'),
      path: grantKey('/ts/v1', member.id),
      role: 'write',
    };
    const record = GrantRecord.active('write', 1, sealNamespaceGrant(ctx, NamespaceCapability.write(topoSecret)));
    await doc.setBytes(rootAuthor, ctx.path, Buffer.from(record.encode()));
    await doc.setBytes(forger, '/ts/v1/grants/forged', Buffer.from('x'));

    // Locally: only the pinned author is visible.
    const local = PolicyDoc.wrap(doc, rootAuthor);
    expect(local.author).toBe(rootAuthor);
    expect((await local.listPrefix('/ts/v1/grants/')).map((e) => Buffer.from(e.key).toString())).toEqual([ctx.path]);
    expect(await local.get('/ts/v1/grants/forged')).toBeUndefined();
    expect(await PolicyDoc.wrap(doc, forger).get('/ts/v1/grants/forged')).toBeDefined();

    // Remotely: import read-only, sync from the root, reconcile. (The root
    // only answers sync for a doc it is live-syncing.)
    await doc.startSync([]);
    const policy = await PolicyDoc.importReadOnly(member.docs(), nsId, rootAuthor);
    await policy.doc().startSync([root.id]);
    const fetched = await until('grant record to sync', () => policy.fetchGrant('/ts/v1', member.id));
    const outcome = reconcileNamespaceGrant(member.exportSecretKey(), ctx, fetched);
    expect(outcome.kind).toBe('active');
    if (outcome.kind === 'active') {
      expect(outcome.value.canWrite).toBe(true);
      expect(Buffer.from(outcome.value.exposeSecret()).equals(topoSecret)).toBe(true);
    }
    // The root's own identity cannot open the member's grant.
    expect(reconcileNamespaceGrant(root.exportSecretKey(), ctx, fetched).kind).toBe('unopenable');
  });
});

describe('topology', () => {
  const nodes: Node[] = [];

  beforeAll(async () => {
    for (let i = 0; i < 3; i++) {
      nodes.push(await Node.start({ ...OFFLINE, enableMonitoring: true, alpns: ['t7/probe'] }));
    }
    await Promise.all(nodes.map(ready));
    for (const x of nodes) for (const y of nodes) if (x !== y) x.native.addNodeAddr(y.native);
  });

  afterAll(async () => {
    await Promise.all(nodes.map((n) => n.close()));
  });

  it('v1: a monitored node sees its connected peers on the same host', async () => {
    const [a, b] = nodes;
    expect(a.topology().hasMonitoring).toBe(true);
    // Any connection is measured: hold one open on a probe ALPN.
    const accepted = b.native.acceptAster();
    const held = await within(a.connect(b.addr, 't7/probe'), 10_000, 'connect');
    await within(accepted, 10_000, 'accept');
    const view = await until('peer view', () => a.topology().peer(b.id));
    expect(view.nodeId).toBe(b.id);
    expect(view.level).toBe('same_host');
    expect(view.levelRank).toBe(0);
    expect(view.direct).toBe(true);
    expect(view.lastMeasured).toBeInstanceOf(Date);
    expect(a.topology().peers().some((p) => p.nodeId === b.id)).toBe(true);
    expect(a.topology().peer('00'.repeat(32))).toBeUndefined();
    void held;
  });

  it('v2: three nodes converge on one cluster', async () => {
    await expect(nodes[0].topology().clusters()).rejects.toThrow(/enable_shared|not joined/);

    const secret = randomBytes(32);
    const ids = nodes.map((n) => n.id);
    for (const n of nodes) {
      await n.topology().enableShared(secret, {
        bootstrap: ids.filter((id) => id !== n.id),
        positionRefreshMs: 500,
        timing: { minHoldMs: 0, livenessTtlMs: 60_000 },
      });
    }
    const sorted = [...ids].sort();
    const cluster = await until(
      'one cluster of three',
      async () => {
        const c = await nodes[0].topology().myCluster();
        return c && c.members.length === 3 ? c : undefined;
      },
      30_000,
    );
    expect([...cluster.members]).toEqual(sorted);
    expect(sorted).toContain(cluster.bridge);

    const snap = await nodes[0].topology().snapshot();
    expect(Object.isFrozen(snap)).toBe(true);
    expect(snap.clusterOf(ids[1])?.members).toEqual(cluster.members);
    expect(snap.separated(ids[0], ids[2])).toBe('connected');
    expect(snap.separated(ids[0], '00'.repeat(32))).toBe('unknown');
    expect(await nodes[0].topology().separated(ids[1], ids[2])).toBe('connected');
  }, 60_000);
});
