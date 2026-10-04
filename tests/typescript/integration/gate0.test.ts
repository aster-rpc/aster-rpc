/**
 * Gate 0 over real QUIC: a node gated in Rust (`GatePolicy`), JS-decided
 * admission (`node.admission()` as an AsyncIterable), and `AsterServer`'s
 * protected mode with a credentialed consumer (the frozen consumer-admission
 * handler admitting into the Rust store).
 */

import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import {
  AdmissionDeniedError,
  AsterClientWrapper,
  AsterServer,
  GatePolicy,
  Node,
  RPC_ALPN,
  SecretKey,
  bytesToHex,
  generateRootKeypair,
  signCredential,
  type ConsumerEnrollmentCredential,
} from '@aster-rpc/aster';

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
    await using server = await Node.start({ ...OFFLINE, alpns: [APP, 'aster.consumer_admission'] }, { gate });
    await using client = await Node.start(OFFLINE);
    await ready(server);
    expect(server.hasHooks).toBe(true);

    const denied = await dial(client, server, APP);
    expect(denied).toMatchObject({ closed: true, code: 403, reason: 'peer not admitted' });

    // Admission ALPNs stay open to un-admitted peers.
    expect((await dial(client, server, 'aster.consumer_admission', 500)).closed).toBe(false);

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

describe('AsterServer protected mode + credentialed consumer', () => {
  let dir: string;
  const saved: Record<string, string | undefined> = {};

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'aster-gate0-'));
    for (const k of Object.keys(process.env)) {
      if (k.startsWith('ASTER_') && k !== 'ASTER_NATIVE_PATH') {
        saved[k] = process.env[k];
        delete process.env[k];
      }
    }
    process.env.ASTER_RELAY_MODE = 'disabled';
    process.env.ASTER_BIND_ADDR = '127.0.0.1:0';
  });

  afterEach(() => {
    delete process.env.ASTER_RELAY_MODE;
    delete process.env.ASTER_BIND_ADDR;
    for (const [k, v] of Object.entries(saved)) if (v !== undefined) process.env[k] = v;
    rmSync(dir, { recursive: true, force: true });
  });

  it('admits a credentialed consumer into the Rust store; others stay gated', async () => {
    const [rootPriv, rootPub] = await generateRootKeypair();
    const rootPubHex = bytesToHex(rootPub);
    const server = new AsterServer({
      services: [],
      allowAllConsumers: false,
      config: { rootPubkey: rootPubHex },
    });
    await server.start();
    const serve = server.serve();
    try {
      expect(server.gate.mode).toBe('protected');
      for (let i = 0; i < 200 && (server as any)._node.directAddresses().length === 0; i++) {
        await new Promise((r) => setTimeout(r, 10));
      }

      // A consumer with a signed credential bound to its endpoint id.
      const clientKey = SecretKey.generate();
      const clientId = SecretKey.nodeId(clientKey);
      const cred: ConsumerEnrollmentCredential = {
        credentialType: 'policy',
        rootPubkey: rootPubHex,
        expiresAt: Math.floor(Date.now() / 1000) + 3600,
        attributes: { 'aster.role': 'reader' },
        endpointId: clientId,
        signature: '',
      };
      cred.signature = await signCredential(cred, rootPriv);
      const credFile = join(dir, 'consumer.cred');
      writeFileSync(credFile, JSON.stringify({
        credential_type: cred.credentialType,
        root_pubkey: cred.rootPubkey,
        expires_at: cred.expiresAt,
        attributes: cred.attributes,
        endpoint_id: cred.endpointId,
        signature: cred.signature,
      }));

      expect(server.gate.isAdmitted(clientId)).toBe(false);
      const client = new AsterClientWrapper({
        address: server.address,
        config: { secretKey: clientKey },
        enrollmentCredentialFile: credFile,
      });
      await client.connect();
      try {
        const rec = server.gate.store.get(clientId)!;
        expect(rec).toBeDefined();
        expect(rec.attributes).toEqual({ 'aster.role': 'reader' });
        expect(rec.source).toBe('credential');
        expect(rec.expiresAt!.getTime()).toBeLessThanOrEqual(cred.expiresAt * 1000);
        // Its RPC connection passed Gate 0 and stays open.
        const rpc = await dial((client as any)._node, (server as any)._node, RPC_ALPN, 500);
        expect(rpc.closed).toBe(false);
      } finally {
        await client.close();
      }

      // A consumer without a credential is denied admission, and Gate 0
      // closes its RPC connection with the default code / reason.
      const anon = new AsterClientWrapper({ address: server.address });
      await expect(anon.connect()).rejects.toBeInstanceOf(AdmissionDeniedError);
      const anonNode = (anon as any)._node as Node;
      expect(await dial(anonNode, (server as any)._node, RPC_ALPN)).toMatchObject({
        closed: true,
        code: 403,
        reason: 'peer not admitted',
      });
      await anon.close();

      // Revocation through the server's gate re-closes the door.
      server.gate.revoke(clientId);
      expect(server.gate.isAdmitted(clientId)).toBe(false);
    } finally {
      await server.close();
      await serve.catch(() => {});
    }
  });
});
