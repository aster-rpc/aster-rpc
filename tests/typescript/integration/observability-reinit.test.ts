import { expect, it } from 'vitest';
import { initObservability, shutdownObservability, metrics, Node, prometheusText } from '@aster-rpc/aster';

it('joins matching init, rejects conflict, and redeclares existing metrics after shutdown', async () => {
  const first = { fromEnv: false, serviceName: 'review-first', exportTraces: false, exportMetrics: false };
  let node: Awaited<ReturnType<typeof Node.start>> | undefined;
  let health: Awaited<ReturnType<NonNullable<typeof node>['startHealth']>> | undefined;
  try {
    await initObservability(first);
    await initObservability(first);
    await expect(initObservability({ ...first, serviceName: 'conflict' })).rejects.toThrow();
    const counter = metrics.counter('review.reinit');
    const attrs = metrics.attributes({ phase: 'retained' });
    node = await Node.start({ relayMode: 'disabled', bindAddr: '127.0.0.1:0' });
    health = await node.startHealth({ port: 0 });
    counter.add(3, attrs);
    expect(prometheusText()).toMatch(/^review_reinit_total\{[^}]*phase="retained"[^}]*\} 3$/m);
    expect(await shutdownObservability()).toBe(true);
    // One matching-init owner remains: its instruments still record.
    counter.add(2, attrs);
    expect(prometheusText()).toMatch(/^review_reinit_total\{[^}]*phase="retained"[^}]*\} 5$/m);
    const retained = health;
    const address = retained.addr, port = retained.port;
    await Promise.all([retained.stop(), retained.stop(), retained.stop()]);
    expect(retained.addr).toBe(address);
    expect(retained.port).toBe(port);
    retained.setHealthy(true); retained.setReady(true);
    expect(retained.isHealthy()).toBe(true);
    const afterStop = retained.check('after-stop');
    expect(retained.isReady()).toBe(false);
    afterStop.set(true);
    expect(retained.isReady()).toBe(true);
    expect(retained.removeCheck('after-stop')).toBe(true);
    await retained.stop();
    health = undefined;
    await node.close(); node = undefined;
    expect(await shutdownObservability()).toBe(true);
    expect(await shutdownObservability()).toBe(false);
    counter.add(1, attrs); // No running pipeline: quiet no-op.
    await initObservability({ ...first, serviceName: 'review-second' });
    expect(retained.addr).toBe(address); // retained stopped object needs no GC
    node = await Node.start({ relayMode: 'disabled', bindAddr: '127.0.0.1:0' });
    health = await node.startHealth({ port }); // concurrent stop fully released the old port
    counter.add(4, attrs);
    expect(prometheusText()).toMatch(/^review_reinit_total\{[^}]*phase="retained"[^}]*\} 4$/m);
  } finally {
    await health?.stop();
    await node?.close();
    while (await shutdownObservability()) { /* release only this test's references */ }
  }
}, 15_000);
