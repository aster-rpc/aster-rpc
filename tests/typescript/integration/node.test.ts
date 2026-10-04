/**
 * Node on `aster::Node` + `AsterConfig`: lifecycle, secret key files
 * (create / reload / conflict) and persistent restarts — through the bare
 * `Node`, `AsterServer` and `AsterClientWrapper`.
 */

import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import {
  AsterClientWrapper,
  AsterServer,
  Node,
  NodeId,
  SecretKey,
} from '@aster-rpc/aster';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;
const KEY_A = '11'.repeat(32);
const KEY_B = '22'.repeat(32);

let dir: string;
const savedEnv: Record<string, string | undefined> = {};

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), 'aster-node-'));
  for (const k of Object.keys(process.env)) {
    if (k.startsWith('ASTER_') && k !== 'ASTER_NATIVE_PATH') {
      savedEnv[k] = process.env[k];
      delete process.env[k];
    }
  }
  process.env.ASTER_RELAY_MODE = 'disabled';
  process.env.ASTER_BIND_ADDR = '127.0.0.1:0';
});

afterEach(() => {
  delete process.env.ASTER_RELAY_MODE;
  delete process.env.ASTER_BIND_ADDR;
  for (const [k, v] of Object.entries(savedEnv)) {
    if (v !== undefined) process.env[k] = v;
  }
  rmSync(dir, { recursive: true, force: true });
});

describe('Node', () => {
  it('starts in memory and closes via await using', async () => {
    let held: Node;
    {
      await using node = await Node.start(OFFLINE);
      held = node;
      expect(NodeId.is(node.id)).toBe(true);
      expect(node.hasHooks).toBe(false);
      expect(node.exportSecretKey()).toHaveLength(32);
      expect(node.closed).toBe(false);
    }
    expect(held.closed).toBe(true);
    await held.close(); // idempotent
  });

  it('pins its id to an explicit secret key', async () => {
    await using node = await Node.start({ ...OFFLINE, secretKey: KEY_A });
    expect(node.id).toBe(SecretKey.nodeId(KEY_A));
    expect(Buffer.from(node.exportSecretKey()).toString('hex')).toBe(KEY_A);
  });

  it('creates a secret key file, then reloads it', async () => {
    const keyFile = join(dir, 'sub', 'node.key');
    let firstId: string;
    {
      await using node = await Node.start({ ...OFFLINE, secretKeyFile: keyFile });
      firstId = node.id;
    }
    expect(existsSync(keyFile)).toBe(true);
    expect(readFileSync(keyFile, 'utf8').trim()).toMatch(/^[0-9a-f]{64}$/);
    if (process.platform !== 'win32') {
      expect(statSync(keyFile).mode & 0o777).toBe(0o600);
    }
    expect(SecretKey.nodeId(SecretKey.readFile(keyFile)!)).toBe(firstId);

    await using again = await Node.start({ ...OFFLINE, secretKeyFile: keyFile });
    expect(again.id).toBe(firstId);
  });

  it('writes an explicit secret key into a missing key file', async () => {
    const keyFile = join(dir, 'node.key');
    await using node = await Node.start({ ...OFFLINE, secretKey: KEY_A, secretKeyFile: keyFile });
    expect(node.id).toBe(SecretKey.nodeId(KEY_A));
    expect(SecretKey.readFile(keyFile)).toBe(KEY_A);
  });

  it('refuses a secret key that conflicts with the key file', async () => {
    const keyFile = join(dir, 'node.key');
    SecretKey.writeFile(keyFile, KEY_A);
    await expect(Node.start({ ...OFFLINE, secretKey: KEY_B, secretKeyFile: keyFile }))
      .rejects.toThrow(/different keys/);
    // The agreeing key is fine.
    await using node = await Node.start({ ...OFFLINE, secretKey: KEY_A, secretKeyFile: keyFile });
    expect(node.id).toBe(SecretKey.nodeId(KEY_A));
  });

  it('rejects a malformed key file', async () => {
    const keyFile = join(dir, 'node.key');
    writeFileSync(keyFile, 'not a key\n');
    await expect(Node.start({ ...OFFLINE, secretKeyFile: keyFile })).rejects.toThrow();
    expect(() => SecretKey.writeFile(keyFile, KEY_A)).toThrow(); // never overwrites
  });

  it('keeps identity and data across a persistent restart', async () => {
    const config = {
      ...OFFLINE,
      storagePath: join(dir, 'store'),
      secretKeyFile: join(dir, 'store', 'node.key'),
    };
    let id: string;
    let hash: string;
    {
      await using node = await Node.start(config);
      id = node.id;
      hash = await node.blobs().addBytes(Buffer.from('persist me'));
    }
    await using node = await Node.start(config);
    expect(node.id).toBe(id);
    expect(Buffer.from(await node.blobs().read(hash)).toString()).toBe('persist me');
  });

  it('connects two nodes by address', async () => {
    await using a = await Node.start({ ...OFFLINE, alpns: ['test/1'] });
    await using b = await Node.start(OFFLINE);
    for (let i = 0; i < 100 && a.directAddresses().length === 0; i++) {
      await new Promise((r) => setTimeout(r, 20));
    }
    const conn = await b.connect(a.addr, 'test/1');
    const accepted = await a.native.acceptAster();
    expect(accepted.alpn()).toBe('test/1');
    conn.close(0, 'done');
  });
});

describe('AsterServer / AsterClientWrapper on Node + AsterConfig', () => {
  it('a persistent server keeps its endpoint id and tickets carry direct addresses', async () => {
    const config = {
      storagePath: join(dir, 'srv'),
      secretKeyFile: join(dir, 'srv', 'node.key'),
    };
    const first = new AsterServer({ services: [], allowAllConsumers: true, config });
    await first.start();
    const id = first.endpointId;
    for (let i = 0; i < 100 && (first as any)._node.directAddresses().length === 0; i++) {
      await new Promise((r) => setTimeout(r, 20));
    }
    expect((first as any)._node.directAddresses().length).toBeGreaterThan(0);
    expect(first.address.startsWith('aster1')).toBe(true);
    await first.close();

    const second = new AsterServer({ services: [], allowAllConsumers: true, config });
    await second.start();
    expect(second.endpointId).toBe(id);
    expect(SecretKey.nodeId(SecretKey.readFile(config.secretKeyFile)!)).toBe(id);
    await second.close();
  });

  it('honours ASTER_SECRET_KEY_FILE from the environment', async () => {
    const keyFile = join(dir, 'env.key');
    SecretKey.writeFile(keyFile, KEY_B);
    process.env.ASTER_SECRET_KEY_FILE = keyFile;
    try {
      const server = new AsterServer({ services: [], allowAllConsumers: true });
      await server.start();
      expect(server.endpointId).toBe(SecretKey.nodeId(KEY_B));
      await server.close();
    } finally {
      delete process.env.ASTER_SECRET_KEY_FILE;
    }
  });

  it('a client .aster-identity key that conflicts with secretKeyFile fails to start', async () => {
    const identity = join(dir, '.aster-identity');
    writeFileSync(identity, `[node]\nsecret_key = "${KEY_A}"\n`);
    const keyFile = join(dir, 'client.key');
    SecretKey.writeFile(keyFile, KEY_B);
    const client = new AsterClientWrapper({
      address: SecretKey.nodeId(KEY_B),
      identity,
      config: { secretKeyFile: keyFile },
    });
    await expect(client.connect()).rejects.toThrow(/different keys/);
    await client.close();
  });
});
