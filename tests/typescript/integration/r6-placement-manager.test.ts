/**
 * R6: the §7.7.3 placement manager with JavaScript providers (policy
 * source, provisioner, topology source — upcall kinds 32 / 33 / 17) against
 * a **real** three-voter group run by `aster-coordination-fixture` (see
 * t8-coordination.test.ts for how to build it; skipped, with a notice,
 * without one). Nothing here changes membership: the passes either find the
 * roles satisfied or stop before submitting.
 */

import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { createInterface } from 'node:readline';
import {
  CoordinationClient,
  Node,
  PlacementManager,
  Upcalls,
  type GroupStatus,
  type PlacementPolicy,
  type ProvisionRequest,
  type TopologyView,
} from '@aster-rpc/aster';

const REPO = path.resolve(__dirname, '../../..');
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;

function fixtureBinary(): string | undefined {
  const exe = process.platform === 'win32' ? '.exe' : '';
  const target = process.env.CARGO_TARGET_DIR ?? path.join(REPO, 'target');
  return [
    process.env.ASTER_COORDINATION_FIXTURE,
    path.join(target, 'debug', `aster-coordination-fixture${exe}`),
    path.join(target, 'release', `aster-coordination-fixture${exe}`),
  ]
    .filter((p): p is string => !!p && existsSync(p))
    .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
}

const FIXTURE = fixtureBinary();
if (!FIXTURE) {
  console.warn('[r6-placement-manager] skipped: no aster-coordination-fixture binary');
}

interface GroupInfo {
  groupId: string;
  authorityDomain: string;
  authorityKey: string;
  rootSecret: string;
  voters: Array<{ nodeId: string; ticket: string }>;
}

describe.skipIf(!FIXTURE)('placement manager with JavaScript providers', () => {
  let group: ChildProcess;
  let info: GroupInfo;
  let tmp: string;
  let rootNode: Node;
  let root: CoordinationClient;
  let status: GroupStatus;

  const newcomer = '7a'.repeat(32);
  /** One cluster per node, every pair corroborated far apart. */
  const view = (ids: string[]): TopologyView => ({
    clusters: [...ids].sort().map((id) => ({ members: [id], bridge: id, witnesses: [id] })),
    farPairs: ids.flatMap((a, i) => ids.slice(i + 1).map((b) => [a, b] as [string, string])),
    separationCoverage: 1,
  });
  const policy = (extra: Partial<PlacementPolicy> = {}): PlacementPolicy => ({
    revision: 1,
    digest: '44'.repeat(32),
    authorityPublicKey: status.authorityPublicKey,
    authorityDomain: status.authorityDomain,
    targetVoters: 3,
    targetRetainedLearners: 0,
    eligible: status.voters.map((nodeId) => ({ nodeId })),
    ...extra,
  });

  beforeAll(async () => {
    tmp = mkdtempSync(path.join(tmpdir(), 'aster-r6-placement-'));
    group = spawn(FIXTURE!, [path.join(tmp, 'voters'), '3', String(0xd6)], { stdio: ['pipe', 'pipe', 'inherit'] });
    const lines = createInterface({ input: group.stdout! });
    info = await new Promise<GroupInfo>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('the voter group did not come up in 90 s')), 90_000);
      group.once('exit', (code) => reject(new Error(`the fixture exited early (${code})`)));
      lines.once('line', (line) => {
        clearTimeout(timer);
        resolve(JSON.parse(line) as GroupInfo);
      });
    });
    rootNode = await Node.start({ ...OFFLINE, secretKey: info.rootSecret, rootPubkey: info.authorityKey });
    root = await CoordinationClient.connect(rootNode, info.groupId, {
      seedTickets: info.voters.map((v) => v.ticket),
      journalDir: path.join(tmp, 'root'),
    });
    status = await root.reads().groupStatus();
  }, 120_000);

  afterAll(async () => {
    root?.close();
    await rootNode?.close();
    if (group && group.exitCode === null) {
      const exited = new Promise<void>((r) => group.once('exit', () => r()));
      group.stdin?.end();
      const timer = setTimeout(() => group.kill('SIGKILL'), 15_000);
      await exited;
      clearTimeout(timer);
    }
    if (tmp) rmSync(tmp, { recursive: true, force: true });
  }, 60_000);

  it('a pass reads the JS policy and topology and finds the current roles satisfied', async () => {
    let policyReads = 0;
    let snapshots = 0;
    using manager = new PlacementManager(root, {
      policy: { policy: () => (policyReads++, policy()) },
      topology: { snapshot: async () => (snapshots++, view(status.voters)) },
      provisioner: {
        provision: () => {
          throw new Error('not expected');
        },
      },
    });
    const report = await manager.reconcile();
    expect(report.reason).toBe('current roles already satisfy policy');
    expect(report.policyRevision).toBe(1);
    expect(report.policyDigest).toBe('44'.repeat(32));
    expect([...report.currentVoters].sort()).toEqual([...status.voters].sort());
    expect(report.separation?.totalPairs).toBe(3);
    expect(policyReads).toBeGreaterThanOrEqual(1);
    expect(snapshots).toBeGreaterThanOrEqual(1);
  });

  it('a candidate without seed material goes to the JS provisioner with the idempotency tuple', async () => {
    const requests: ProvisionRequest[] = [];
    using manager = new PlacementManager(root, {
      policy: {
        policy: () =>
          policy({
            eligible: [...status.voters, newcomer].map((nodeId) => ({ nodeId })),
            excluded: [status.voters[0]],
          }),
      },
      topology: { snapshot: () => view([...status.voters, newcomer]) },
      provisioner: {
        provision: async (req) => {
          requests.push(req);
          throw new Error('no capacity in that zone');
        },
      },
    });
    await expect(manager.reconcile()).rejects.toThrow(/no capacity in that zone/);
    expect(requests).toHaveLength(1);
    expect(requests[0]).toMatchObject({ groupId: info.groupId, policyDigest: '44'.repeat(32), nodeId: newcomer });
    if (status.membershipLogId) expect(requests[0].membershipLogId).toEqual(status.membershipLogId);
  });

  it('a failing or slow JS provider rejects the pass (policy: a timeout is an error, not a fallback)', async () => {
    using failing = new PlacementManager(root, {
      policy: { policy: () => policy() },
      topology: {
        snapshot: () => {
          throw new Error('topology offline');
        },
      },
      provisioner: { provision: () => ({ nodeId: newcomer, ticket: 't' }) },
    });
    await expect(failing.reconcile()).rejects.toThrow(/topology offline/);

    const upcalls = new Upcalls({ policies: { placement_policy: { timeoutMs: 200 } } });
    using slow = new PlacementManager(root, {
      upcalls,
      policy: { policy: () => new Promise<PlacementPolicy>(() => {}) },
      topology: { snapshot: () => view(status.voters) },
      provisioner: { provision: () => ({ nodeId: newcomer, ticket: 't' }) },
    });
    await expect(slow.reconcile()).rejects.toThrow(/timed out/);
    upcalls.close();
  });
});
