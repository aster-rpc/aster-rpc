/**
 * Enrollment through the high-level path only: `AsterServer` and
 * `AsterClientWrapper` configured by `AsterConfig`'s `enrollment` section.
 * The TypeScript port of `aster/tests/enrollment_aster_server.rs`.
 *
 * The root is played by the test-hooks `TestEnrollmentRoot` (root operations
 * belong to the Rust CLI, never a binding); everything the application
 * writes is configuration and `requires`.
 */

import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  AsterClientWrapper,
  AsterServer,
  Rpc,
  RpcError,
  SecretKey,
  ServerStream,
  Service,
  StatusCode,
  WireType,
  anyOf,
  type AsterConfig,
  type CallContext,
  type EnrollmentConfig,
} from '@aster-rpc/aster';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.js';

const native: any = tryLoadNative();
const available = !!native?.TestEnrollmentRoot;
if (!available) {
  console.warn('enrollment tests need the test-hooks addon: `bun run build:test-hooks` in bindings/typescript/native');
}

const FLEET = 'mission-control';
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;

@WireType('enroll/OpsRequest')
class OpsRequest {
  note = '';
  constructor(init?: Partial<OpsRequest>) { if (init) Object.assign(this, init); }
}

@WireType('enroll/OpsReply')
class OpsReply {
  role = '';
  constructor(init?: Partial<OpsReply>) { if (init) Object.assign(this, init); }
}

/** `status` requires `ops.status`; `logs` requires `ops.logs` or `ops.admin`; `ping` is open. Each echoes the caller's `aster.role`. */
@Service({ name: 'Ops', version: 1 })
class Ops {
  @Rpc({ request: OpsRequest, response: OpsReply })
  async ping(_req: OpsRequest, ctx: CallContext): Promise<OpsReply> {
    return new OpsReply({ role: ctx.attributes?.['aster.role'] ?? '' });
  }

  @Rpc({ request: OpsRequest, response: OpsReply, requires: 'ops.status' })
  async status(_req: OpsRequest, ctx: CallContext): Promise<OpsReply> {
    return new OpsReply({ role: ctx.attributes?.['aster.role'] ?? '' });
  }

  @Rpc({ request: OpsRequest, response: OpsReply, requires: anyOf('ops.logs', 'ops.admin') })
  async logs(_req: OpsRequest, ctx: CallContext): Promise<OpsReply> {
    return new OpsReply({ role: ctx.attributes?.['aster.role'] ?? '' });
  }

  /** A server stream gated like `logs`: one reply, then the stream ends. */
  @ServerStream({ request: OpsRequest, response: OpsReply, requires: anyOf('ops.logs', 'ops.admin') })
  async *tail(_req: OpsRequest, ctx: CallContext): AsyncGenerator<OpsReply> {
    yield new OpsReply({ role: ctx.attributes?.['aster.role'] ?? '' });
  }
}

/** Register the payload types with a codec (what `aster-gen`'s BUILD_ALL_TYPES does). */
let typeInfos: any[] | undefined;
function buildAllTypes(_fory: any, Type: any, codec: any): Map<string, any> {
  // The server and every in-process client share one Fory runtime: build
  // the struct infos once, register them with each codec.
  if (!typeInfos) {
    const struct = (cls: any, typeName: string, props: Record<string, any>) => {
      const t = Type.struct({ namespace: 'enroll', typeName }, props, { withConstructor: true });
      t.initMeta(cls);
      return t;
    };
    typeInfos = [
      struct(OpsRequest, 'OpsRequest', { note: Type.string() }),
      struct(OpsReply, 'OpsReply', { role: Type.string() }),
    ];
  }
  for (const t of typeInfos) codec.registerType(t);
  return new Map();
}
const generated = { SERVICES: [] as any[], WIRE_TYPES: [] as any[], buildAllTypes };

let dir: string;
let root: any;

beforeAll(async () => {
  dir = mkdtempSync(join(tmpdir(), 'aster-ts-enroll-'));
  if (available) root = await native.TestEnrollmentRoot.start(FLEET);
});

afterAll(async () => {
  await root?.close();
  rmSync(dir, { recursive: true, force: true });
});

function serverEnrollment(extra: EnrollmentConfig = {}): EnrollmentConfig {
  return {
    audience: FLEET,
    fleetNamespace: root.fleetNamespace,
    statePath: join(dir, `server-${Math.random().toString(16).slice(2)}.json`),
    limits: { reconciliationIntervalMs: 200 },
    ...extra,
  };
}

/** A server node key and the root-issued credential `aster enroll node` writes for it. */
function serverConfig(extra: Partial<AsterConfig> = {}, enrollment: EnrollmentConfig = {}): Partial<AsterConfig> {
  const key = SecretKey.generate();
  return {
    ...OFFLINE,
    secretKey: key,
    rootPubkey: root.rootPubkey,
    enrollmentCredentialChain: root.issue(SecretKey.nodeId(key)),
    enrollment: serverEnrollment(enrollment),
    ...extra,
  };
}

async function startServer(config: Partial<AsterConfig>): Promise<AsterServer> {
  const server = new AsterServer({ services: [new Ops()], config, generated });
  try {
    await server.start();
  } catch (e) {
    await server.close();
    throw e;
  }
  return server;
}

/** A client node whose credential file carries `roles`, as `aster enroll node --attributes` writes it. */
function clientConfig(roles: string): Partial<AsterConfig> {
  const key = SecretKey.generate();
  const nodeId = SecretKey.nodeId(key);
  const file = join(dir, `${nodeId.slice(0, 12)}.cred`);
  writeFileSync(file, `${root.issue(nodeId, roles)}\n`);
  return {
    ...OFFLINE,
    secretKey: key,
    rootPubkey: root.rootPubkey,
    enrollmentCredentialFile: file,
    enrollment: { audience: FLEET, fleetNamespace: root.fleetNamespace },
  };
}

async function connect(server: AsterServer, config: Partial<AsterConfig>) {
  const client = new AsterClientWrapper({ address: server.address, config, generated });
  await client.connect();
  return { client, ops: await client.client(Ops) as any };
}

async function role(ops: any, method: 'ping' | 'status' | 'logs'): Promise<string> {
  const reply = await ops[method](new OpsRequest({ note: method }));
  return reply.role;
}

async function tail(ops: any): Promise<string[]> {
  const roles: string[] = [];
  for await (const reply of ops.tail(new OpsRequest({ note: 'tail' }))) roles.push(reply.role);
  return roles;
}

async function denied(p: Promise<unknown>): Promise<boolean> {
  try {
    await p;
    return false;
  } catch (e) {
    return e instanceof RpcError && e.code === StatusCode.PERMISSION_DENIED;
  }
}

async function refused(p: Promise<unknown>): Promise<boolean> {
  try {
    await Promise.race([p, new Promise((_, reject) => setTimeout(() => reject(new Error('timed out')), 10_000))]);
    return false;
  } catch {
    return true;
  }
}

describe.skipIf(!available)('Enrollment on AsterServer / AsterClient from AsterConfig', () => {
  it('credential roles gate requires; root revocation refuses the next call and closes the connection', async () => {
    // No replicas: the server holds the fleet namespace and the root
    // publishes into it, as `aster enroll revoke --peer <server>` does.
    const server = await startServer(serverConfig());
    try {
      expect(server.enrolled).toBe(true);

      const operator = await connect(server, clientConfig('ops.status, ops.logs'));
      expect(operator.client.enrolled).toBe(true);
      expect(await role(operator.ops, 'ping')).toBe('ops.logs,ops.status');
      expect(await role(operator.ops, 'status')).toBe('ops.logs,ops.status');
      expect(await role(operator.ops, 'logs')).toBe('ops.logs,ops.status');
      expect(await tail(operator.ops)).toEqual(['ops.logs,ops.status']);
      // The roles are what `requires` sees for that peer, on every call.
      expect(server.peerAttributes(operator.client.node!.id)['aster.role']).toBe('ops.logs,ops.status');
      // The client discovered the server's services from its registry.
      expect(operator.client.services.map((s) => s.name)).toContain('Ops');

      const ingest = await connect(server, clientConfig('ops.ingest'));
      expect(await role(ingest.ops, 'ping')).toBe('ops.ingest');
      expect(await denied(role(ingest.ops, 'status'))).toBe(true);
      expect(await denied(role(ingest.ops, 'logs'))).toBe(true);
      expect(await denied(tail(ingest.ops))).toBe(true);

      // No enrollment at all: Gate 0 refuses the connection itself.
      const stranger = new AsterClientWrapper({ address: server.address, config: { ...OFFLINE }, generated });
      try {
        let reached = false;
        try {
          await stranger.connect();
          reached = !(await refused(role(await stranger.client(Ops) as any, 'ping')));
        } catch {
          reached = false;
        }
        expect(reached).toBe(false);
      } finally {
        await stranger.close();
      }

      // The root delivers a revocation of the operator into the fleet
      // namespace the server holds.
      await root.revoke(operator.client.node!.id, server.address);
      const deadline = Date.now() + 20_000;
      let revoked = false;
      while (Date.now() < deadline) {
        if (await refused(role(operator.ops, 'status'))) { revoked = true; break; }
        await new Promise((r) => setTimeout(r, 200));
      }
      expect(revoked, 'the server revokes the operator').toBe(true);
      expect(await refused(role(operator.ops, 'ping')), 'connection closed').toBe(true);
      // Unaffected peers keep working.
      expect(await role(ingest.ops, 'ping')).toBe('ops.ingest');

      await operator.client.close();
      await ingest.client.close();
    } finally {
      await server.close();
    }
  }, 120_000);

  it('a config-driven issuer renews the credentials it signed, across several lifetimes', async () => {
    const key = SecretKey.generate();
    const delegationFile = join(dir, 'issuer.chain');
    writeFileSync(delegationFile, `${root.delegate(SecretKey.nodeId(key))}\n`);
    const server = await startServer({
      ...serverConfig({ secretKey: key, enrollmentCredentialChain: root.issue(SecretKey.nodeId(key)) }),
      enrollment: serverEnrollment({
        replicas: [root.ticket],
        issuerDelegationFile: delegationFile,
        issuerLifetimeSecs: 6,
      }),
    });
    const nativeServer = (server as any)._rpcServer.dispatchServer;
    try {
      // The server's issuer signs this client's credential.
      const config = clientConfig('ops.status');
      const nodeId = SecretKey.nodeId(config.secretKey as string);
      const issued: string = await nativeServer.testIssueCredential(nodeId, 'ops.status');
      writeFileSync(config.enrollmentCredentialFile!, `${issued}\n`);

      const { client, ops } = await connect(server, config);
      expect(await client.isRenewing(), 'the server signed it, so it renews').toBe(true);
      const until = Date.now() + (3 * 6 + 2) * 1000;
      while (Date.now() < until) {
        expect(await role(ops, 'status')).toBe('ops.status');
        await new Promise((r) => setTimeout(r, 500));
      }
      expect(await client.credential()).not.toBe(issued);
      await client.close();
    } finally {
      await server.close();
    }
  }, 120_000);

  it('misconfiguration fails closed with a clear error; nothing runs open', async () => {
    const refusedStart = async (config: Partial<AsterConfig>, expectText: string) => {
      const server = new AsterServer({ services: [new Ops()], config, generated });
      try {
        await expect(server.start()).rejects.toThrow(expectText);
      } finally {
        await server.close();
      }
    };
    const good = serverConfig();
    const without = (k: keyof AsterConfig): Partial<AsterConfig> => {
      const c = { ...good };
      delete c[k];
      return c;
    };

    await refusedStart({ ...good, enrollment: { ...good.enrollment, audience: undefined } }, 'audience is required');
    await refusedStart(without('rootPubkey'), 'no trusted root');
    await refusedStart({ ...good, enrollment: { ...good.enrollment, statePath: undefined } }, 'keeps denials durably');
    await refusedStart(without('enrollmentCredentialChain'), 'no credential');
    // A file path where the inline chain belongs.
    await refusedStart({ ...good, enrollmentCredentialChain: '/etc/aster/server.cred' }, 'ASTER_ENROLLMENT_CREDENTIAL)');
    // A credential issued for another key.
    await refusedStart({ ...good, enrollmentCredentialChain: root.issue(SecretKey.nodeId(SecretKey.generate())) }, 'enrollment:');
    await refusedStart({ ...good, enrollment: { ...good.enrollment, policy: 'allow_everyone' as any } }, 'policy');
    await refusedStart({ ...good, enrollment: { ...good.enrollment, fleetNamespace: 'not-hex' } }, 'fleet_namespace');
    // Enabled with nothing else set: still refused, never open.
    await refusedStart({ ...good, enrollment: { enabled: true } }, 'audience is required');
    // A configured root key file that does not exist.
    await refusedStart({ ...without('rootPubkey'), rootPubkeyFile: join(dir, 'missing.pub') }, 'does not exist');

    // A client: no credential, and server-only issuer settings.
    const server = await startServer(serverConfig());
    try {
      const noCredential = clientConfig('ops.status');
      delete noCredential.enrollmentCredentialFile;
      const a = new AsterClientWrapper({ address: server.address, config: noCredential, generated });
      await expect(a.connect()).rejects.toThrow('no credential');
      await a.close();
      const issuer = clientConfig('ops.status');
      issuer.enrollment = { ...issuer.enrollment, issuerDelegationFile: join(dir, 'x') };
      const b = new AsterClientWrapper({ address: server.address, config: issuer, generated });
      await expect(b.connect()).rejects.toThrow('server-only');
      await b.close();
    } finally {
      await server.close();
    }
  }, 120_000);
});
