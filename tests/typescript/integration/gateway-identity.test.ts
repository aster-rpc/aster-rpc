/**
 * Gateway identity (calls on behalf of a user) through the high-level path
 * only: `AsterServer` and `AsterClientWrapper` configured by `AsterConfig`'s
 * `identity` / `remoteConfig` sections and the Enrollment credentials. The
 * TypeScript port of `bindings/go/identity_test.go` (Rust
 * `aster/tests/enrollment_gateway_identity.rs`, `enrollment_provider_tokens.rs`,
 * `node_config.rs`).
 *
 * The root is the test-hooks `TestEnrollmentRoot`.
 */

import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { generateKeyPairSync, sign, type KeyObject } from 'node:crypto';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  AMR_PASSWORD,
  AMR_WEBAUTHN,
  AsterClientWrapper,
  AsterConfig,
  AsterServer,
  CONFIG_ADMIN_ROLE,
  CallContext,
  GATEWAY_ROLE,
  IDENTITY_PUBLISHER_ROLE,
  Rpc,
  RpcError,
  SecretKey,
  ServerStream,
  Service,
  StatusCode,
  WireType,
  bearerToken,
  mergeConfig,
  type EnrollmentConfig,
} from '@aster-rpc/aster';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.js';

const native: any = tryLoadNative();
const available = !!native?.TestEnrollmentRoot;
if (!available) {
  console.warn('gateway identity tests need the test-hooks addon: `bun run build:test-hooks` in bindings/typescript/native');
}

const FLEET = 'gateway-fleet';
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;

@WireType('test.gateway/WhoRequest')
class WhoRequest {
  n = 0;
  constructor(init?: Partial<WhoRequest>) { if (init) Object.assign(this, init); }
}

@WireType('test.gateway/WhoReply')
class WhoReply {
  principal = '';
  peer = '';
  actor = '';
  amr = '';
  roles = '';
  issuer = '';
  tier = '';
  name = '';
  constructor(init?: Partial<WhoReply>) { if (init) Object.assign(this, init); }
}

function who(ctx: CallContext): WhoReply {
  const tier = ctx.nodeAttribute('tier');
  return new WhoReply({
    principal: ctx.principal ?? '',
    peer: ctx.peer ?? '',
    actor: ctx.actor ?? '',
    amr: ctx.authMethods.join(','),
    roles: ctx.attributes['aster.role'] ?? '',
    issuer: ctx.onBehalfOf?.issuer ?? '',
    tier: tier ? String(tier.value) : '',
    name: ctx.nodeProperties.name,
  });
}

/** `whoami` is open; `read` and `tail` need `notes.read`; `publish` also a passkey sign-in. */
@Service({ name: 'Notes', version: 1 })
class Notes {
  @Rpc({ request: WhoRequest, response: WhoReply })
  async whoami(_req: WhoRequest, ctx: CallContext): Promise<WhoReply> {
    return who(ctx);
  }

  @Rpc({ request: WhoRequest, response: WhoReply, requires: 'notes.read' })
  async read(_req: WhoRequest, ctx: CallContext): Promise<WhoReply> {
    return who(ctx);
  }

  @Rpc({ request: WhoRequest, response: WhoReply, requires: 'notes.read' })
  async publish(_req: WhoRequest, ctx: CallContext): Promise<WhoReply> {
    ctx.requireAuthMethod(AMR_WEBAUTHN);
    return who(ctx);
  }

  /** A server stream that reads its context through `CallContext.current()`. */
  @ServerStream({ request: WhoRequest, response: WhoReply, requires: 'notes.read' })
  async *tail(req: WhoRequest): AsyncGenerator<WhoReply> {
    for (let i = 0; i < req.n; i++) {
      const ctx = CallContext.current();
      if (!ctx) throw new Error('no CallContext in the stream');
      yield who(ctx);
    }
  }
}

let typeInfos: any[] | undefined;
function buildAllTypes(_fory: any, Type: any, codec: any): Map<string, any> {
  if (!typeInfos) {
    const struct = (cls: any, typeName: string, props: Record<string, any>) => {
      const t = Type.struct({ namespace: 'test.gateway', typeName }, props, { withConstructor: true });
      t.initMeta(cls);
      return t;
    };
    const s = Type.string();
    typeInfos = [
      struct(WhoRequest, 'WhoRequest', { n: Type.int32() }),
      struct(WhoReply, 'WhoReply', {
        principal: s, peer: s, actor: s, amr: s, roles: s, issuer: s, tier: s, name: s,
      }),
    ];
  }
  for (const t of typeInfos) codec.registerType(t);
  return new Map();
}
const generated = { SERVICES: [] as any[], WIRE_TYPES: [] as any[], buildAllTypes };

let dir: string;
let root: any;
const cleanups: (() => Promise<void>)[] = [];

beforeAll(async () => {
  dir = mkdtempSync(join(tmpdir(), 'aster-ts-gateway-'));
  if (available) root = await native.TestEnrollmentRoot.start(FLEET);
});

afterEach(async () => {
  while (cleanups.length) await cleanups.pop()!().catch(() => {});
});

afterAll(async () => {
  await root?.close();
  rmSync(dir, { recursive: true, force: true });
});

function serverEnrollment(): EnrollmentConfig {
  return {
    audience: FLEET,
    fleetNamespace: root.fleetNamespace,
    statePath: join(dir, `server-${Math.random().toString(16).slice(2)}.json`),
    limits: { reconciliationIntervalMs: 200 },
  };
}

async function startNotes(identity: AsterConfig['identity'], extra: { acceptOnBehalfOf?: boolean } = {}) {
  const key = SecretKey.generate();
  const server = new AsterServer({
    services: [new Notes()],
    generated,
    ...extra,
    config: {
      ...OFFLINE,
      secretKey: key,
      rootPubkey: root.rootPubkey,
      enrollmentCredentialChain: root.issue(SecretKey.nodeId(key)),
      enrollment: serverEnrollment(),
      identity,
    },
  });
  cleanups.push(() => server.close());
  await server.start();
  return { server, id: SecretKey.nodeId(key) };
}

/** A client whose credential carries `roles` and, when given, an issuer delegation. */
async function connect(server: AsterServer, roles: string, issuable?: string) {
  const key = SecretKey.generate();
  const nodeId = SecretKey.nodeId(key);
  const chain: string = root.issue(nodeId, roles, undefined, issuable);
  const file = join(dir, `${nodeId.slice(0, 12)}.cred`);
  writeFileSync(file, `${chain}\n`);
  const client = new AsterClientWrapper({
    address: server.address,
    generated,
    config: {
      ...OFFLINE,
      secretKey: key,
      rootPubkey: root.rootPubkey,
      enrollmentCredentialFile: file,
      enrollment: { audience: FLEET, fleetNamespace: root.fleetNamespace },
    },
  });
  cleanups.push(() => client.close());
  await client.connect();
  return { client, id: nodeId, chain, notes: (await client.client(Notes)) as any };
}

async function code(p: Promise<unknown>): Promise<number | undefined> {
  try {
    await p;
    return undefined;
  } catch (e) {
    if (e instanceof RpcError) return e.code;
    throw e;
  }
}

async function collect(stream: AsyncIterable<WhoReply>): Promise<WhoReply[]> {
  const out: WhoReply[] = [];
  for await (const r of stream) out.push(r);
  return out;
}

const req = (n = 1) => new WhoRequest({ n });

describe('gateway identity config', () => {
  it('bearerToken reads a Bearer header only', () => {
    expect(bearerToken('Bearer abc.def')).toBe('abc.def');
    expect(bearerToken('Basic x')).toBeUndefined();
  });

  it('[identity] / [remote_config] load from env and keep their sources through code', () => {
    const env: Record<string, string> = {
      ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF: 'true',
      ASTER_IDENTITY_TRUSTED_ISSUERS: '[{"iss":"https://idp.test","audiences":["notes"],"roles_claim":"groups","role_map":{"readers":"notes.read"}}]',
      ASTER_IDENTITY_CLOCK_LEEWAY_SECS: '0',
      ASTER_IDENTITY_PUBLISHER: '{"issuers":["https://idp.test"],"jwks_url":{"https://idp.test":"https://idp.test/jwks"},"refresh_secs":600}',
      ASTER_REMOTE_CONFIG_LOCK: 'identity',
    };
    const saved = Object.fromEntries(Object.keys(env).map((k) => [k, process.env[k]]));
    Object.assign(process.env, env);
    try {
      const cfg = AsterConfig.fromEnv();
      const id = cfg.identity!;
      expect(id.acceptOnBehalfOf).toBe(true);
      expect(id.trustedIssuers).toHaveLength(1);
      expect(id.trustedIssuers![0]!.roleMap).toEqual({ readers: 'notes.read' });
      expect(id.trustedIssuers![0]!.maxTokenLifetimeSecs).toBe(900);
      expect(id.clockLeewaySecs).toBe(0);
      expect(id.edgeTokenMaxLifetimeSecs).toBe(300);
      expect(id.publisher?.refreshSecs).toBe(600);
      expect(id.publisher?.jwksUrl).toEqual({ 'https://idp.test': 'https://idp.test/jwks' });
      expect(cfg.remoteConfig).toEqual({ enabled: true, lock: ['identity'] });
      expect(Object.keys(id.sources ?? {}).sort()).toEqual([
        'identity.accept_on_behalf_of', 'identity.clock_leeway_secs', 'identity.trusted_issuers',
      ]);
      expect(id.sources!['identity.accept_on_behalf_of']!.source).toBe('env');

      // Through Rust and back: unchanged values keep `env`.
      const back = AsterConfig.resolve(cfg);
      expect(back.identity?.sources).toEqual(id.sources);
      expect(back.identity?.publisher?.issuers).toEqual(['https://idp.test']);
      // A value changed in code no longer claims the environment.
      const changed = AsterConfig.resolve(mergeConfig(cfg, { identity: { acceptOnBehalfOf: false } }));
      expect(changed.identity?.acceptOnBehalfOf).toBe(false);
      expect(changed.identity?.sources?.['identity.accept_on_behalf_of']).toBeUndefined();
      expect(changed.identity?.sources?.['identity.trusted_issuers']?.source).toBe('env');
    } finally {
      for (const [k, v] of Object.entries(saved)) {
        if (v === undefined) delete process.env[k];
        else process.env[k] = v;
      }
    }
  });

  it('a bad [identity] section fails the server start', async () => {
    if (!available) return;
    await expect(startNotes({ trustedIssuers: [{ iss: 'https://idp.test', audiences: [] }] }))
      .rejects.toThrow(/identity/);
  });
});

describe.skipIf(!available)('gateway identity on AsterServer / AsterClient', () => {
  it('an edge-minted token reaches handlers (unary and streaming); NodeConfig set / ack / clear', async () => {
    const { server, id: serverId } = await startNotes({ acceptOnBehalfOf: true });
    const edge = await connect(server, `${GATEWAY_ROLE},${CONFIG_ADMIN_ROLE}`, 'notes.read');

    const me: WhoReply = await edge.notes.whoami(req());
    expect(me).toMatchObject({ principal: edge.id, peer: edge.id, actor: '', amr: '', issuer: '' });
    expect(await code(edge.notes.read(req()))).toBe(StatusCode.PERMISSION_DENIED);

    const token = await edge.client.mintUserToken('alice', ['notes.read'], [AMR_WEBAUTHN], 60);
    const asAlice = edge.notes.withOptions({ onBehalfOf: token });
    const alice: WhoReply = await asAlice.publish(req());
    expect(alice).toMatchObject({
      principal: `${edge.id}|alice`, peer: edge.id, actor: edge.id,
      amr: AMR_WEBAUTHN, roles: 'notes.read', issuer: edge.id,
    });
    // A server stream reads the same identity through CallContext.current().
    const streamed = await collect(asAlice.tail(req(2)));
    expect(streamed).toHaveLength(2);
    for (const r of streamed) {
      expect(r).toMatchObject({ principal: `${edge.id}|alice`, actor: edge.id, amr: AMR_WEBAUTHN });
    }

    const pwd = await edge.client.mintUserToken('bob', ['notes.read'], [AMR_PASSWORD], 60);
    expect(await code(edge.notes.publish(req(), { onBehalfOf: pwd }))).toBe(StatusCode.PERMISSION_DENIED);
    expect((await edge.notes.read(req(), { onBehalfOf: pwd })).principal).toBe(`${edge.id}|bob`);
    await expect(edge.client.mintUserToken('bob', ['notes.admin'], [AMR_PASSWORD], 60)).rejects.toThrow();
    expect(await edge.client.noticeKid('https://idp.test', 'k1')).toBe(false);
    expect(await edge.client.identityPublisherStatus()).toBeNull();

    // NodeConfig from the admin edge.
    const admin = edge.client.nodeConfig();
    const view = await admin.get();
    expect(view.nodeId).toBe(serverId);
    const accept = view.settings.find((s) => s.key === 'identity.accept_on_behalf_of');
    expect(accept).toMatchObject({ value: true, source: 'code' });

    const ack = await admin.set({
      name: 'notes-1', tags: ['eu'],
      attributes: { tier: { type: 'int', value: 3 } },
      identity: { acceptOnBehalfOf: false },
    });
    expect(ack).toMatchObject({ complete: true, generation: 1 });
    const after: WhoReply = await edge.notes.whoami(req());
    expect(after).toMatchObject({ tier: '3', name: 'notes-1' });
    const props = server.nodeProperties();
    expect(props.generation).toBe(1);
    expect(props.tags).toContain('eu');
    expect(props.attributes.tier).toEqual({ type: 'int', value: 3 });
    expect(await code(asAlice.read(req()))).toBe(StatusCode.PERMISSION_DENIED);
    expect((await admin.ack(1, 1000)).complete).toBe(true);
    expect((await admin.get()).settings.find((s) => s.key === 'identity.accept_on_behalf_of'))
      .toMatchObject({ value: false, source: 'override' });
    expect(await code(admin.set({ sections: { network: { bind_port: 1 } } }))).toBe(StatusCode.PERMISSION_DENIED);

    expect((await admin.clear()).complete).toBe(true);
    const cleared: WhoReply = await asAlice.read(req());
    expect(cleared).toMatchObject({ actor: edge.id, tier: '', name: '' });

    // Without aster.config-admin Gate 3 refuses NodeConfig; without
    // aster.gateway the entry is refused.
    const plain = await connect(server, 'notes.read');
    expect(await code(plain.client.nodeConfig().get())).toBe(StatusCode.PERMISSION_DENIED);
    expect(await code(plain.notes.read(req(), { onBehalfOf: token }))).toBe(StatusCode.PERMISSION_DENIED);
  }, 120_000);

  it('acceptOnBehalfOf: false on the server refuses the entry', async () => {
    const { server } = await startNotes({ acceptOnBehalfOf: true }, { acceptOnBehalfOf: false });
    const edge = await connect(server, `${GATEWAY_ROLE},notes.read`, 'notes.read');
    const token = await edge.client.mintUserToken('alice', ['notes.read'], [AMR_PASSWORD], 60);
    expect((await edge.notes.read(req())).actor).toBe('');
    expect(await code(edge.notes.read(req(), { onBehalfOf: token }))).toBe(StatusCode.PERMISSION_DENIED);
  }, 60_000);

  it('a provider ES256 token verifies with a key set a publisher node wrote', async () => {
    const iss = 'https://idp.test';
    const { server, id: serverId } = await startNotes({
      acceptOnBehalfOf: true,
      trustedIssuers: [{ iss, audiences: ['notes'], rolesClaim: 'groups', roleMap: { readers: 'notes.read' } }],
    });
    const idp = testIdP(iss);

    // The publisher writes the key set into the fleet namespace as its own
    // node, carrying its credential, and syncs it to the server.
    const publisher = await connect(server, IDENTITY_PUBLISHER_ROLE);
    const docs = publisher.client.node!.docs() as any;
    const fleet = await docs.openOrImportWriteNamespace(root.fleetSecret);
    const record = {
      version: 1, iss, source_url: `${iss}/jwks`, fetched_at: Math.floor(Date.now() / 1000),
      jwks: { keys: [idp.jwk] }, credential: publisher.chain,
    };
    const key = `aster/identity/v1/issuers/${Buffer.from(iss).toString('base64url')}/jwks`;
    await fleet.setBytes(await docs.defaultAuthor(), key, Buffer.from(JSON.stringify(record)));
    await fleet.startSync([serverId]);

    const edge = await connect(server, GATEWAY_ROLE);
    const token = idp.token({ sub: 'alice', aud: 'notes', groups: ['readers', 'other'], amr: ['pwd'] });
    const deadline = Date.now() + 30_000;
    let alice: WhoReply | undefined;
    while (!alice) {
      try {
        alice = await edge.notes.read(req(), { onBehalfOf: token });
      } catch (e) {
        // UNAUTHENTICATED until the key set has synced.
        if (!(e instanceof RpcError) || e.code !== StatusCode.UNAUTHENTICATED || Date.now() > deadline) throw e;
        await new Promise((r) => setTimeout(r, 200));
      }
    }
    expect(alice).toMatchObject({
      principal: `${iss}|alice`, actor: edge.id, roles: 'notes.read', issuer: iss, amr: 'pwd',
    });
    // Another audience, or another key under the same kid, is refused.
    expect(await code(edge.notes.read(req(), { onBehalfOf: idp.token({ sub: 'alice', aud: 'other' }) })))
      .toBe(StatusCode.UNAUTHENTICATED);
    const forged = testIdP(iss).token({ sub: 'alice', aud: 'notes' });
    expect(await code(edge.notes.read(req(), { onBehalfOf: forged }))).toBe(StatusCode.UNAUTHENTICATED);
  }, 120_000);
});

/** An identity provider signing ES256 tokens with one P-256 key (kid `k1`). */
function testIdP(iss: string): { jwk: Record<string, string>; token: (claims: Record<string, unknown>) => string } {
  const { publicKey, privateKey } = generateKeyPairSync('ec', { namedCurve: 'P-256' });
  const pub = publicKey.export({ format: 'jwk' }) as Record<string, string>;
  const jwk = { kty: 'EC', crv: 'P-256', kid: 'k1', alg: 'ES256', use: 'sig', x: pub.x!, y: pub.y! };
  const b64 = (v: unknown) => Buffer.from(JSON.stringify(v)).toString('base64url');
  const token = (claims: Record<string, unknown>): string => {
    const now = Math.floor(Date.now() / 1000);
    const input = `${b64({ alg: 'ES256', typ: 'JWT', kid: 'k1' })}.${b64({ ...claims, iss, iat: now, exp: now + 300 })}`;
    const sig = sign('sha256', Buffer.from(input), { key: privateKey as KeyObject, dsaEncoding: 'ieee-p1363' });
    return `${input}.${sig.toString('base64url')}`;
  };
  return { jwk, token };
}
