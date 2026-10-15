/**
 * The registry's best-effort sync wait treats a failed `sync_finished`
 * round as "not synced yet" and keeps waiting until its deadline.
 */

import { describe, expect, it } from 'vitest';
import { waitForInitialSync } from '../../../bindings/typescript/packages/aster/src/runtime.js';

type Ev = { kind: string; syncOk?: boolean; syncOrigin?: string; syncError?: string };

/** Yields the given events, then stays quiet. */
function receiver(events: Ev[]) {
  return {
    recv: (): Promise<Ev | null> =>
      events.length ? Promise.resolve(events.shift()!) : new Promise<Ev | null>(() => {}),
  };
}

const emptyDoc = { queryKeyPrefix: async () => [] };
const failed: Ev = { kind: 'sync_finished', syncOk: false, syncOrigin: 'connect', syncError: 'aborted' };
const completed: Ev = { kind: 'sync_finished', syncOk: true, syncOrigin: 'accept' };

describe('waitForInitialSync', () => {
  it('waits to the deadline after a failed round', async () => {
    const start = Date.now();
    expect(await waitForInitialSync(emptyDoc, receiver([failed]), 500)).toBe(false);
    expect(Date.now() - start).toBeGreaterThanOrEqual(490);
  });

  it('stops at a completed round after a failed one', async () => {
    expect(await waitForInitialSync(emptyDoc, receiver([failed, completed]), 5000)).toBe(true);
  });

  it('stops when an entry shows up while events are quiet', async () => {
    const doc = { queryKeyPrefix: async () => [{}] };
    expect(await waitForInitialSync(doc, receiver([failed]), 5000)).toBe(true);
  });
});
