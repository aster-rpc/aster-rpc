/**
 * Gate 0 over real QUIC: a node gated in Rust (`GatePolicy`) and JS-decided
 * admission (`node.admission()` as an AsyncIterable) — the low-level escape
 * hatches. `AsterServer` admits peers with Enrollment
 * (`enrollment-aster-server.test.ts`).
 */

import { describe, expect, it } from 'vitest';
import { GatePolicy, Node } from '@aster-rpc/aster';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;
const APP = 'test/1';

async function ready(node: Node): Promise<void> {
  for (let i = 0; i < 200 && node.directAddresses().length === 0; i++) {
    await new Promise((r) => setTimeout(r, 10));
  }
}

type Outcome = { closed: true; code?: number; reason?: string } | { closed: false };

/** Dial `server` on `alpn`; report whether the server closes it within `ms`. */
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

describe('Gate 0 in Rust (GatePolicy on a node)', () => {
  it('denies un-admitted peers with the gate code/reason; admit and revoke apply live', async () => {
    const gate = GatePolicy.protected();
    await using server = await Node.start({ ...OFFLINE, alpns: [APP, 'aster.admission'] }, { gate });
    await using client = await Node.start(OFFLINE);
    await ready(server);
    expect(server.hasHooks).toBe(true);

    const denied = await dial(client, server, APP);
    expect(denied).toMatchObject({ closed: true, code: 403, reason: 'peer not admitted' });

    // Admission ALPNs stay open to un-admitted peers.
    expect((await dial(client, server, 'aster.admission', 500)).closed).toBe(false);

    gate.setReject(451, 'go away');
    expect(await dial(client, server, APP)).toMatchObject({ closed: true, code: 451, reason: 'go away' });

    gate.admit(client.id);
    expect((await dial(client, server, APP, 500)).closed).toBe(false);

    gate.revoke(client.id);
    expect((await dial(client, server, APP)).closed).toBe(true);

    gate.mode = 'open_dev';
    expect((await dial(client, server, APP, 500)).closed).toBe(false);
  });

  it('setGate(null) hands decisions to admission(); setGate(gate) takes them back', async () => {
    await using server = await Node.start({ ...OFFLINE, alpns: [APP], enableHooks: true });
    await using client = await Node.start(OFFLINE);
    await ready(server);
    const gate = GatePolicy.protected();
    server.setGate(gate);
    expect((await dial(client, server, APP)).closed).toBe(true);
    server.setGate(null);
    // No admission() iterator open: the hook failure mode (fail_open) applies.
    expect((await dial(client, server, APP, 500)).closed).toBe(false);
  });

  it('setGate / admission() need hooks', async () => {
    await using node = await Node.start(OFFLINE);
    expect(node.hasHooks).toBe(false);
    expect(() => node.setGate(new GatePolicy())).toThrow(/admission hooks/);
    expect(() => node.admission()).toThrow(/admission hooks/);
  });
});

describe('admission() as an AsyncIterable', () => {
  it('accept / reject(code, reason) reach the peer', async () => {
    await using server = await Node.start({ ...OFFLINE, alpns: [APP], enableHooks: true, hookTimeoutMs: 5000 });
    await using client = await Node.start(OFFLINE);
    await ready(server);
    const requests = server.admission();
    expect(() => server.admission()).toThrow(/already/);

    const seen: { peer: string; alpn: string; direction: string }[] = [];
    let n = 0;
    const loop = (async () => {
      for await (const req of requests) {
        seen.push({ peer: req.peer, alpn: req.alpn, direction: req.direction });
        if (n++ === 0) req.reject(499, 'not today');
        else req.accept();
        expect(req.answered).toBe(true);
        expect(() => req.accept()).toThrow(/already answered/);
        if (n === 2) break;
      }
    })();

    expect(await dial(client, server, APP)).toMatchObject({ closed: true, code: 499, reason: 'not today' });
    expect((await dial(client, server, APP, 500)).closed).toBe(false);
    await loop;
    expect(seen).toEqual([
      { peer: client.id, alpn: APP, direction: 'inbound' },
      { peer: client.id, alpn: APP, direction: 'inbound' },
    ]);

    // Iterator closed by `break`: later requests get the failure mode
    // (fail_open) at once instead of waiting for the hook timeout.
    const t0 = Date.now();
    expect((await dial(client, server, APP, 500)).closed).toBe(false);
    expect(Date.now() - t0).toBeLessThan(4000);
  });
});
