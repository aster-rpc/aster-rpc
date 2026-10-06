/**
 * Plan T8 — the wrapped coordination client against a **real** three-voter
 * group (production profile) over loopback Iroh.
 *
 * The voters are Rust-only (plan D7), so the group runs in a child process:
 * `aster-coordination-fixture` (bindings/typescript/native/fixtures), built
 * from `aster::coordination::testing::GroupFixture` — the same fixture the
 * Rust client gates use. Build it with
 *
 *   cargo build -p aster-transport-napi --features coordination-fixture \
 *       --bin aster-coordination-fixture
 *
 * or point `ASTER_COORDINATION_FIXTURE` at a binary. Without one the suite is
 * skipped (with a notice), never silently passed.
 */

import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { createInterface } from 'node:readline';
import { randomBytes } from 'node:crypto';
import {
  CoordinationAwaitingAckError,
  CoordinationClient,
  CoordinationJournalLockedError,
  CoordinationRejectedError,
  CoordinationRights,
  CoordinationRpcError,
  CoordinationUnresolvedError,
  Node,
  fenceOf,
  placementPlan,
  type AuditRecord,
  type ResourceKeyInput,
} from '@aster-rpc/aster';

import { coordinationFixture } from '../helpers/coordination-fixture.ts';

const REPO = path.resolve(__dirname, '../../..');
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;

const FIXTURE = coordinationFixture(REPO);
if (!FIXTURE) {
  console.warn(
    '[t8-coordination] skipped: no aster-coordination-fixture binary ' +
      '(cargo build -p aster-transport-napi --features coordination-fixture --bin aster-coordination-fixture)',
  );
}

interface GroupInfo {
  groupId: string;
  genesisHash: string;
  authorityDomain: string;
  authorityKey: string;
  rootSecret: string;
  voters: Array<{ nodeId: string; ticket: string }>;
}

async function startGroup(dir: string, voters: number, seed: number): Promise<{ child: ChildProcess; info: GroupInfo }> {
  const child = spawn(FIXTURE!, [dir, String(voters), String(seed)], { stdio: ['pipe', 'pipe', 'inherit'] });
  const lines = createInterface({ input: child.stdout! });
  const info = await new Promise<GroupInfo>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('the voter group did not come up in 90 s')), 90_000);
    child.once('exit', (code) => reject(new Error(`the fixture exited early (${code})`)));
    lines.once('line', (line) => {
      clearTimeout(timer);
      resolve(JSON.parse(line) as GroupInfo);
    });
  });
  return { child, info };
}

async function stopGroup(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null) return;
  const exited = new Promise<void>((r) => child.once('exit', () => r()));
  child.stdin?.end();
  const timer = setTimeout(() => child.kill('SIGKILL'), 15_000);
  await exited;
  clearTimeout(timer);
}

describe.skipIf(!FIXTURE)('coordination client against a live three-voter group', () => {
  let group: ChildProcess;
  let info: GroupInfo;
  let tmp: string;
  let rootNode: Node;
  let workerNode: Node;
  let contenderNode: Node;
  let root: CoordinationClient;
  let worker: CoordinationClient;
  const seeds = () => info.voters.map((v) => v.ticket);
  const journal = (name: string) => path.join(tmp, name);
  const res = (key: string): ResourceKeyInput => ({ namespace: 't8.coordination', key });

  beforeAll(async () => {
    tmp = mkdtempSync(path.join(tmpdir(), 'aster-t8-'));
    ({ child: group, info } = await startGroup(path.join(tmp, 'voters'), 3, 0xc7));
    // ROOT (the group's immutable super-administrator) is the endpoint whose
    // secret is the deployment ROOT key; it has no implicit resource rights.
    rootNode = await Node.start({ ...OFFLINE, secretKey: info.rootSecret, rootPubkey: info.authorityKey });
    workerNode = await Node.start({ ...OFFLINE, secretKey: randomBytes(32), rootPubkey: info.authorityKey });
    contenderNode = await Node.start({ ...OFFLINE, secretKey: randomBytes(32), rootPubkey: info.authorityKey });
    root = await CoordinationClient.connect(rootNode, info.groupId, { seedTickets: seeds(), journalDir: journal('root') });
    worker = await CoordinationClient.connect(workerNode, info.groupId, {
      seedTickets: seeds(),
      journalDir: journal('worker'),
    });
    for (const key of ['head', 'lease', 'restart']) {
      const acl = await root.reads().inspectAcl(res(key));
      const granted = await root.setGrant(res(key), workerNode.id, CoordinationRights.HOLDER, acl.aclRevision);
      expect(granted.outcome).toBe('grant_updated');
      await granted.acknowledge();
    }
    const acl = await root.reads().inspectAcl(res('lease'));
    const contenderGrant = await root.setGrant(res('lease'), contenderNode.id, CoordinationRights.HOLDER, acl.aclRevision);
    await contenderGrant.acknowledge();
  }, 120_000);

  afterAll(async () => {
    root?.close();
    worker?.close();
    await rootNode?.close();
    await workerNode?.close();
    await contenderNode?.close();
    if (group) await stopGroup(group);
    if (tmp) rmSync(tmp, { recursive: true, force: true });
  }, 60_000);

  it('reports a production group status cut', async () => {
    const status = await root.reads().groupStatus();
    expect(status.groupId).toBe(info.groupId);
    expect(status.genesisHash).toBe(info.genesisHash);
    expect(status.authorityDomain).toBe(info.authorityDomain);
    expect(status.authorityPublicKey).toBe(info.authorityKey);
    expect(status.deploymentProfileCode).toBe(1);
    expect([...status.voters].sort()).toEqual(info.voters.map((v) => v.nodeId).sort());
    expect(status.fatalCode).toBe(0);

    const acl = await root.reads().inspectAcl(res('head'));
    expect(acl.grants).toEqual([{ subjectNodeId: workerNode.id, rights: CoordinationRights.HOLDER }]);
    const admins = await root.reads().inspectAdmins();
    expect(admins.administrators.length).toBeGreaterThan(0);
  });

  it('acquire, renew, compare-and-set, read, release — each resolved and acknowledged', async () => {
    const acquired = await worker.acquire(res('head'), 30_000);
    expect(acquired.outcome).toBe('granted');
    expect(acquired.reply.consumed).toBe(true);
    expect(acquired.snapshot?.holder).toBe(workerNode.id);
    expect(acquired.pending).toBe(true);

    // The delivery contract: no new sequence until acknowledged.
    await expect(worker.acquire(res('other'), 30_000)).rejects.toBeInstanceOf(CoordinationAwaitingAckError);
    await acquired.acknowledge();
    expect(acquired.pending).toBe(false);
    await expect(acquired.acknowledge()).rejects.toMatchObject({ kind: 'closed' });

    const fence = fenceOf(acquired.snapshot!);
    const renewed = await worker.renew(fence);
    expect(renewed.outcome).toBe('renewed');
    expect(renewed.sequence).toBe(acquired.sequence + 1);
    await renewed.acknowledge();

    const before = await worker.reads().readLinearizable(res('head'));
    const set = await worker.compareAndSet(fence, before.valueRevision, 'v1');
    expect(set.outcome).toBe('mutated');
    await set.acknowledge();

    // A stale expected revision is a committed typed rejection: still a
    // resolved result that must be acknowledged.
    const stale = await worker.compareAndSet(fence, before.valueRevision, 'v2');
    expect(stale.rejected).toBe(true);
    expect(stale.outcome).toBe('rejected');
    expect(stale.reason).toBe('value_revision_mismatch');
    expect(stale.rejection()).toBeInstanceOf(CoordinationRejectedError);
    await stale.acknowledge();

    const mine = await worker.reads().readLinearizable(res('head'));
    expect(Buffer.from(mine.value!).toString()).toBe('v1');
    expect(mine.held).toBe(true);
    expect(mine.epoch).toBe(fence.epoch);

    const released = await worker.release(fence);
    expect(released.outcome).toBe('released');
    await released.acknowledge();
    expect((await worker.reads().readLinearizable(res('head'))).held).toBe(false);

    // A fence from the released epoch is refused, typed.
    const late = await worker.renew(fence);
    expect(late.rejected).toBe(true);
    expect(['not_held', 'epoch_mismatch']).toContain(late.reason);
    await late.acknowledge();
  });

  it('a lease renews in Rust, mutates under its fence, and hands off atomically', async () => {
    // Renewing every 100 ms on purpose: the compare-and-set and the final
    // handoff below land while renewals are in flight, and ending the lease
    // mid-renewal must not strand the shared journal.
    const outcome = await worker.acquireLease(res('lease'), { ttlMs: 6_000, renewIntervalMs: 100, holderMarginMs: 500 });
    if (!outcome.granted) throw new Error(`expected a grant, got ${outcome.reason}`);
    const lease = outcome.lease;
    expect(lease.status).toBe('held');
    const first = lease.fence();
    expect(first?.holder).toBe(workerNode.id);
    expect(lease.resource.namespace).toBe('t8.coordination');

    // Another entitled actor is refused while the lease is held.
    const rival = await CoordinationClient.connect(contenderNode, info.groupId, {
      seedTickets: seeds(),
      journalDir: journal('contender'),
    });
    const contender = await rival.acquireLease(res('lease'));
    expect(contender.granted).toBe(false);
    if (!contender.granted) {
      expect(contender.reason).toBe('held_by_other');
      expect(contender.snapshot.held).toBe(true);
    }
    rival.close();

    // Outlive the TTL: the renew loop keeps it.
    await new Promise((r) => setTimeout(r, 7_000));
    expect(lease.status).toBe('held');
    expect(lease.fence()?.epoch).toBe(first!.epoch);

    const current = await worker.reads().readLinearizable(res('lease'));
    const mutated = await lease.compareAndSet(current.valueRevision, 'next');
    expect(mutated.outcome).toBe('mutated');
    await expect(lease.compareAndSet(current.valueRevision, 'stale')).rejects.toBeInstanceOf(CoordinationRejectedError);

    const handed = await lease.releaseWithValue(mutated.snapshot!.valueRevision, 'final');
    expect(handed.outcome).toBe('released');
    expect(lease.status).toBe('released');
    expect(lease.fence()).toBeNull();
    await lease.release(); // idempotent

    const after = await worker.reads().readLinearizable(res('lease'));
    expect(after.held).toBe(false);
    expect(Buffer.from(after.value!).toString()).toBe('final');
    // The journal was left free by the auto-acknowledging lease path.
    expect(await worker.unacknowledged()).toBeNull();
  }, 30_000);

  it('a pre-proposal permission denial is a typed RPC error that leaves the sequence open', async () => {
    const actor = await Node.start({ ...OFFLINE, secretKey: randomBytes(32), rootPubkey: info.authorityKey });
    const dir = journal('ungranted');
    let client = await CoordinationClient.connect(actor, info.groupId, { seedTickets: seeds(), journalDir: dir });
    const originalClientId = await client.clientId();
    try {
      // No ACL row for this actor: refused before proposal (PERMISSION_DENIED).
      const denied = await client.acquire(res('head'), 30_000).catch((e: unknown) => e);
      expect(denied).toBeInstanceOf(CoordinationRpcError);
      expect((denied as CoordinationRpcError).code).toBe(7);
      // Nothing committed; the journaled request stays open until resumed.
      await expect(client.acquire(res('lease'), 30_000)).rejects.toBeInstanceOf(CoordinationUnresolvedError);
      await expect(client.resume()).rejects.toBeInstanceOf(CoordinationRpcError);
      // The lease adapter surfaces the same typed status.
      const second = await CoordinationClient.connect(actor, info.groupId, {
        seedTickets: seeds(),
        journalDir: journal('ungranted-lease'),
      });
      const viaLease = await second.acquireLease(res('head')).catch((e: unknown) => e);
      second.close();
      expect(viaLease).toBeInstanceOf(CoordinationRpcError);

      // Restore permission after closing the process handle. Reopening the
      // same journal must drive its saved acquire request, without planning a
      // replacement request or consuming a different sequence.
      client.close();
      const acl = await root.reads().inspectAcl(res('head'));
      const grant = await root.setGrant(res('head'), actor.id, CoordinationRights.HOLDER, acl.aclRevision);
      expect(grant.outcome).toBe('grant_updated');
      await grant.acknowledge();

      client = await CoordinationClient.connect(actor, info.groupId, { seedTickets: seeds(), journalDir: dir });
      const resumed = await client.resume();
      expect(resumed).not.toBeNull();
      expect(resumed!.outcome).toBe('granted');
      expect(resumed!.reply.consumed).toBe(true);
      expect(await client.clientId()).toBe(originalClientId);
      expect(resumed!.reply.clientId).toBe(originalClientId);
      expect(resumed!.reply.requestHash).toMatch(/^[0-9a-f]{64}$/);
      expect(resumed!.snapshot?.resource.namespace).toBe(res('head').namespace);
      expect(Buffer.from(resumed!.snapshot!.resource.key).toString()).toBe('head');

      // Until acknowledged, resume returns the same durable result. Once
      // acknowledged, the next mutation can use the recovered fence and must
      // advance this journal by exactly one sequence.
      const replay = await client.resume();
      expect(replay?.reply).toEqual(resumed!.reply);
      await resumed!.acknowledge();
      const released = await client.release(fenceOf(resumed!.snapshot!));
      expect(released.outcome).toBe('released');
      expect(released.sequence).toBe(resumed!.sequence + 1);
      expect(released.reply.clientId).toBe(resumed!.reply.clientId);
      await released.acknowledge();
    } finally {
      client.close();
      await actor.close();
    }
  });

  it('redelivers an unacknowledged result after a restart over the same journal', async () => {
    const dir = journal('restart-client');
    const actor = await Node.start({ ...OFFLINE, secretKey: randomBytes(32), rootPubkey: info.authorityKey });
    try {
      const acl = await root.reads().inspectAcl(res('restart'));
      const grant = await root.setGrant(res('restart'), actor.id, CoordinationRights.HOLDER, acl.aclRevision);
      await grant.acknowledge();

      const first = await CoordinationClient.connect(actor, info.groupId, { seedTickets: seeds(), journalDir: dir });
      const clientId = await first.clientId();
      const resolved = await first.acquire(res('restart'), 30_000);
      expect(resolved.outcome).toBe('granted');
      const saved = resolved.reply;

      // One process owns the journal.
      await expect(
        CoordinationClient.connect(actor, info.groupId, { seedTickets: seeds(), journalDir: dir }),
      ).rejects.toBeInstanceOf(CoordinationJournalLockedError);

      // "Crash": close without acknowledging. The handle goes inert.
      first.close();
      expect(resolved.pending).toBe(false);

      const second = await CoordinationClient.connect(actor, info.groupId, { seedTickets: seeds(), journalDir: dir });
      expect(await second.clientId()).toBe(clientId);
      await expect(second.acquire(res('other'), 30_000)).rejects.toBeInstanceOf(CoordinationAwaitingAckError);

      const redelivered = await second.unacknowledged();
      expect(redelivered).not.toBeNull();
      expect(redelivered!.reply).toEqual(saved);
      // resume() redelivers the same awaiting result too.
      const viaResume = await second.resume();
      expect(viaResume!.reply).toEqual(saved);
      await redelivered!.acknowledge();
      // The other handle names the same, now-acknowledged entry: refused.
      await expect(viaResume!.acknowledge()).rejects.toThrow();

      expect(await second.unacknowledged()).toBeNull();
      expect(await second.resume()).toBeNull();

      const released = await second.release(fenceOf(saved.snapshot!));
      expect(released.outcome).toBe('released');
      expect(released.sequence).toBe(saved.sequence + 1);
      expect(released.reply.clientId).toBe(clientId);
      await released.acknowledge();

      const probe = await second.reads().probeClient(actor.id, clientId);
      expect(probe.exists).toBe(true);
      expect(probe.lastSequence).toBe(released.sequence);
      second.close();
    } finally {
      await actor.close();
    }
  }, 60_000);

  it('streams the audit log in ascending, paged order', async () => {
    const reads = root.reads();
    const all: AuditRecord[] = [];
    for await (const row of reads.auditStream()) all.push(row);
    expect(all.length).toBeGreaterThan(5);
    for (let i = 1; i < all.length; i++) {
      expect(all[i]!.logId.index).toBeGreaterThan(all[i - 1]!.logId.index);
    }
    const ops = new Set(all.map((r) => r.operationCode));
    expect(ops.has(1)).toBe(true); // acquire
    expect(ops.has(3)).toBe(true); // compare_and_set
    expect(ops.has(7)).toBe(true); // set_grant
    expect(all.some((r) => r.actorNodeId === workerNode.id && r.resource?.namespace === 't8.coordination')).toBe(true);

    // Small pages walk the same rows.
    const paged: AuditRecord[] = [];
    for await (const row of reads.auditStream({ pageSize: 3 })) paged.push(row);
    expect(paged.map((r) => r.logId.index)).toEqual(all.map((r) => r.logId.index));

    // A cursor and a limit.
    const tail: AuditRecord[] = [];
    for await (const row of reads.auditStream({ after: all[1]!.logId, limit: 2 })) tail.push(row);
    expect(tail.map((r) => r.logId.index)).toEqual(all.slice(2, 4).map((r) => r.logId.index));

    // The worker is no administrator.
    await expect(worker.reads().auditPage(undefined, 10)).rejects.toMatchObject({ kind: 'rpc' });
    await expect(reads.auditPage(undefined, 0)).rejects.toMatchObject({ kind: 'invalid' });
  });

  it('exports a verified recovery snapshot', async () => {
    const { manifest, body } = await root.reads().recoveryExport();
    expect(manifest.totalLen).toBe(body.length);
    expect(manifest.blake3Checksum).toMatch(/^[0-9a-f]{64}$/);
    const file = path.join(tmp, 'export.bin');
    const toFile = await root.reads().recoveryExportToFile(file);
    expect(statSync(file).size).toBe(toFile.totalLen);
    expect(existsSync(`${file}.partial`)).toBe(false);
  });

  it('plans placement from the live status cut', async () => {
    const status = await root.reads().groupStatus();
    const monitor = await Node.start({ ...OFFLINE, enableMonitoring: true });
    try {
      await monitor.topology().enableShared(randomBytes(32), { timing: { minHoldMs: 0 } });
      const snapshot = await monitor.topology().snapshot();
      const rec = placementPlan(
        {
          groupId: status.groupId,
          genesisHash: status.genesisHash,
          deploymentProfileCode: status.deploymentProfileCode,
          ...(status.membershipLogId ? { membershipLogId: status.membershipLogId } : {}),
          currentVoters: status.voters,
          currentLearners: status.learners,
          activeMembershipIntent: status.pendingOperationId != null,
          policy: {
            revision: 1,
            digest: '44'.repeat(32),
            authorityPublicKey: status.authorityPublicKey,
            authorityDomain: status.authorityDomain,
            targetVoters: 3,
            targetRetainedLearners: 0,
            eligible: status.voters.map((nodeId) => ({ nodeId })),
          },
        },
        snapshot,
      );
      expect(['stable', 'degraded']).toContain(rec.kind);
    } finally {
      await monitor.close();
    }
  });
});
