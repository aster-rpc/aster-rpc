/**
 * Plan T8 (coordination client) — the parts that need no voter group: the
 * §5.2 code tables against the Rust enums, typed-error mapping, argument
 * validation at the NAPI boundary, and the pure placement planner.
 */

import { describe, expect, it } from 'vitest';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import {
  COORDINATION_OUTCOMES,
  COORDINATION_REASONS,
  CoordinationClient,
  CoordinationClosedError,
  CoordinationError,
  CoordinationJournalLockedError,
  CoordinationRejectedError,
  CoordinationRights,
  Node,
  fenceOf,
  newOperationId,
  placementPlan,
  type PlacementInput,
} from '@aster-rpc/aster';
import { loadNative } from '../../../bindings/typescript/packages/aster/src/native.js';
import { coordinationError } from '../../../bindings/typescript/packages/aster/src/coordination.js';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;
const GROUP = 'ab'.repeat(32);

describe('coordination: code tables', () => {
  it('the reason table matches the Rust enum exactly', () => {
    const native = Object.fromEntries(loadNative().coordinationReasonCodes().map((r) => [r.code, r.name]));
    expect(native).toEqual({ ...COORDINATION_REASONS });
  });

  it('the outcome table matches the Rust enum exactly', () => {
    const native = Object.fromEntries(loadNative().coordinationOutcomeCodes().map((r) => [r.code, r.name]));
    expect(native).toEqual({ ...COORDINATION_OUTCOMES });
  });

  it('rights are the §8.1 bits', () => {
    expect(CoordinationRights.HOLDER).toBe(
      CoordinationRights.READ |
        CoordinationRights.ACQUIRE |
        CoordinationRights.RENEW |
        CoordinationRights.MUTATE |
        CoordinationRights.RELEASE,
    );
    expect(CoordinationRights.REVOKE).toBe(0x20);
  });
});

describe('coordination: typed errors', () => {
  it('maps every tag to its class', () => {
    const cases: Array<[string, string, string]> = [
      ['[coordination:awaiting_ack] x', 'CoordinationAwaitingAckError', 'awaiting_ack'],
      ['[coordination:unresolved] x', 'CoordinationUnresolvedError', 'unresolved'],
      ['[coordination:journal_fatal] x', 'CoordinationJournalFatalError', 'journal_fatal'],
      ['[coordination:journal_locked] x', 'CoordinationJournalLockedError', 'journal_locked'],
      ['[coordination:unavailable] x', 'CoordinationUnavailableError', 'unavailable'],
      ['[coordination:closed] x', 'CoordinationClosedError', 'closed'],
      ['[coordination:invalid] x', 'CoordinationError', 'invalid'],
      ['[coordination:rpc:7] x', 'CoordinationRpcError', 'rpc'],
    ];
    for (const [message, name, kind] of cases) {
      const e = coordinationError(new Error(message)) as CoordinationError;
      expect(e).toBeInstanceOf(CoordinationError);
      expect(e.name).toBe(name);
      expect(e.kind).toBe(kind);
      expect(e.message).toBe('x');
    }
    expect((coordinationError(new Error('[coordination:rpc:7] x')) as { code: number }).code).toBe(7);
    const plain = new Error('something else');
    expect(coordinationError(plain)).toBe(plain);
  });

  it('a rejection carries its reason and reply', () => {
    const e = new CoordinationRejectedError({
      groupId: GROUP,
      clientId: '00'.repeat(16),
      sequence: 3,
      requestHash: '00'.repeat(32),
      consumed: true,
      dispositionCode: 1,
      outcome: 'rejected',
      outcomeCode: 100,
      reason: 'held_by_other',
      reasonCode: 1,
      aclRevision: 0,
      adminRevision: 0,
    });
    expect(e.kind).toBe('rejected');
    expect(e.reason).toBe('held_by_other');
    expect(e.reply.sequence).toBe(3);
  });

  it('fenceOf refuses an unheld snapshot', () => {
    expect(() =>
      fenceOf({
        groupId: GROUP,
        resource: { namespace: 'n', key: new Uint8Array([1]) },
        epoch: 1,
        held: false,
        ttlMs: 0,
        leaseRevision: 0,
        valueRevision: 0,
        aclRevision: 0,
        valueRedacted: false,
      }),
    ).toThrow(CoordinationError);
  });

  it('newOperationId is 16 random bytes', () => {
    const a = newOperationId();
    expect(a).toMatch(/^[0-9a-f]{32}$/);
    expect(newOperationId()).not.toBe(a);
  });
});

describe('coordination: client lifecycle without a group', () => {
  it('validates the group id, locks the journal, and closes', async () => {
    const dir = mkdtempSync(path.join(tmpdir(), 'aster-coord-'));
    await using node = await Node.start(OFFLINE);
    try {
      await expect(CoordinationClient.connect(node, 'nope', { journalDir: dir })).rejects.toMatchObject({
        name: 'CoordinationError',
        kind: 'invalid',
      });
      await expect(
        CoordinationClient.connect(node, GROUP, { journalDir: dir, seedTickets: ['not-a-ticket'] }),
      ).rejects.toBeInstanceOf(CoordinationError);

      const client = await CoordinationClient.connect(node, GROUP, { journalDir: dir });
      expect(client.groupId).toBe(GROUP);
      expect(client.localId).toBe(node.id);
      expect(await client.clientId()).toMatch(/^[0-9a-f]{32}$/);
      expect(await client.unacknowledged()).toBeNull();
      expect(await client.resume()).toBeNull();

      // One lifetime lock per actor/group journal.
      await expect(CoordinationClient.connect(node, GROUP, { journalDir: dir })).rejects.toBeInstanceOf(
        CoordinationJournalLockedError,
      );

      // Bad arguments are refused before anything is journaled.
      await expect(client.setGrant({ namespace: 'n', key: 'k' }, 'zz', CoordinationRights.READ, 0)).rejects.toMatchObject(
        { kind: 'invalid' },
      );
      await expect(client.acquire({ namespace: 'n', key: 'k' }, -1)).rejects.toMatchObject({ kind: 'invalid' });

      client.close();
      expect(client.closed).toBe(true);
      client.close(); // idempotent
      await expect(client.acquire({ namespace: 'n', key: 'k' }, 1000)).rejects.toBeInstanceOf(CoordinationClosedError);

      // close() released the lock: the same journal reopens.
      const again = await CoordinationClient.connect(node, GROUP, { journalDir: dir });
      expect(await again.unacknowledged()).toBeNull();
      again.close();
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe('coordination: placement planner (pure)', () => {
  it('judges the policy and the membership facts', async () => {
    await using node = await Node.start({ ...OFFLINE, enableMonitoring: true });
    await node.topology().enableShared(new Uint8Array(32).fill(7), { timing: { minHoldMs: 0 } });
    const snapshot = await node.topology().snapshot();
    const ids = [node.id, 'a1'.repeat(32), 'b2'.repeat(32)];
    const base: PlacementInput = {
      groupId: GROUP,
      genesisHash: '11'.repeat(32),
      deploymentProfileCode: 1, // production: exactly 3 or 5 voters
      currentVoters: ids,
      policy: {
        revision: 1,
        digest: '22'.repeat(32),
        authorityPublicKey: '33'.repeat(32),
        authorityDomain: 'test.domain',
        targetVoters: 3,
        targetRetainedLearners: 0,
        eligible: ids.map((nodeId) => ({ nodeId })),
      },
    };

    const bad = placementPlan({ ...base, policy: { ...base.policy, targetVoters: 4 } }, snapshot);
    expect(bad.kind).toBe('invalid_policy');

    const pinnedIneligible = placementPlan(
      { ...base, policy: { ...base.policy, pinnedVoters: ['c3'.repeat(32)] } },
      snapshot,
    );
    expect(pinnedIneligible.kind).toBe('invalid_policy');

    expect(placementPlan({ ...base, activeMembershipIntent: true }, snapshot).kind).toBe('converging');

    const settled = placementPlan(base, snapshot);
    expect(['stable', 'degraded']).toContain(settled.kind);
    if (settled.kind === 'stable' || settled.kind === 'degraded') {
      expect(settled.separation.totalPairs).toBe(3);
    }

    expect(() => placementPlan(base, { ...snapshot })).toThrow(CoordinationError);
  });
});
