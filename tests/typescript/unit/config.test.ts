/**
 * AsterConfig: the Rust env / TOML / identity loaders through NAPI, and the
 * typed options-object round trip.
 */

import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import {
  AsterConfig,
  getProducerTokens,
  isEnrollmentEnabled,
  loadIdentity,
  mergeConfig,
  resolveRootPubkey,
} from '@aster-rpc/aster';

/** Every ASTER_* variable a test might set or that could leak in from CI. */
const ENV_KEYS = Object.keys(process.env).filter((k) => k.startsWith('ASTER_'))
  .concat([
    'ASTER_LOG_FORMAT', 'ASTER_LOG_LEVEL', 'ASTER_LOG_MASK', 'ASTER_HEALTH_PORT',
    'ASTER_HEALTH_HOST', 'ASTER_HEALTH_ENABLED', 'ASTER_ALLOW_ALL_CONSUMERS', 'ASTER_ENROLLMENT_CREDENTIAL_IID',
    'ASTER_ENROLLMENT_AUDIENCE', 'ASTER_ENROLLMENT_FLEET_NAMESPACE', 'ASTER_ENROLLMENT_POLICY',
    'ASTER_ENROLLMENT_ROLE_ATTRIBUTE', 'ASTER_ENROLLMENT_HANDSHAKE_TIMEOUT_MS',
    'ASTER_ENROLLMENT_CREDENTIAL', 'ASTER_ENROLLMENT_REPLICAS', 'ASTER_ROOT_ROTATIONS', 'ASTER_ROOT_SEQ',
    'ASTER_RELAY_MODE', 'ASTER_RELAY_URLS', 'ASTER_BIND_ADDR', 'ASTER_SECRET_KEY',
    'ASTER_SECRET_KEY_FILE', 'ASTER_STORAGE_PATH', 'ASTER_GC_INTERVAL_MS',
    'ASTER_HOOK_TIMEOUT_MS', 'ASTER_TRANSPORT_RECEIVE_WINDOW', 'ASTER_ENABLE_HOOKS',
    'ASTER_PORTMAPPER_CONFIG', 'ASTER_BLOB_ZSTD_LEVEL', 'ASTER_REUSE_LAST_PORT',
  ]);

const KEY_A = '11'.repeat(32);
const ROOT = 'ab'.repeat(32);

let saved: Record<string, string | undefined> = {};
let dir: string;

beforeEach(() => {
  saved = {};
  for (const k of ENV_KEYS) {
    saved[k] = process.env[k];
    delete process.env[k];
  }
  dir = mkdtempSync(join(tmpdir(), 'aster-config-'));
});

afterEach(() => {
  for (const [k, v] of Object.entries(saved)) {
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
  rmSync(dir, { recursive: true, force: true });
});

describe('AsterConfig.fromEnv', () => {
  it('returns the Rust defaults with every field filled in', () => {
    const c = AsterConfig.fromEnv();
    expect(c.logFormat).toBe('text');
    expect(c.logLevel).toBe('info');
    expect(c.logMask).toBe(true);
    // Retired with the legacy consumer admission.
    expect('allowAllConsumers' in c).toBe(false);
    expect('allowAllProducers' in c).toBe(false);
    expect('enrollmentCredentialIid' in c).toBe(false);
    // Enrollment is off, with the Rust defaults filled in.
    expect(isEnrollmentEnabled(c)).toBe(false);
    expect(c.rootSeq).toBe(0);
    expect(c.rootRotations).toEqual([]);
    expect(c.enrollment?.roles).toEqual({
      attribute: 'aster.role', scopesAsRoles: false, scopes: {}, tags: {}, includeAttributes: true,
    });
    expect(c.relayMode).toBe('default');
    expect(c.hookTimeoutMs).toBe(5000);
    expect(c.hookFailureMode).toBe('fail_closed');
    expect(c.gcIntervalMs).toBe(0);
    expect(c.enableHooks).toBe(false);
    expect(c.healthPort).toBe(0);
    expect(c.healthHost).toBe('127.0.0.1');
    expect(c.storagePath).toBeUndefined();
    expect(c.secretKey).toBeUndefined();
  });

  it('reads ASTER_* variables, including u64 knobs above u32', () => {
    process.env.ASTER_LOG_FORMAT = 'json';
    process.env.ASTER_HEALTH_PORT = '8080';
    process.env.ASTER_RELAY_URLS = 'https://r1.example, https://r2.example';
    process.env.ASTER_SECRET_KEY = KEY_A;
    process.env.ASTER_SECRET_KEY_FILE = '/tmp/node.key';
    process.env.ASTER_TRANSPORT_RECEIVE_WINDOW = '10000000000';
    process.env.ASTER_PORTMAPPER_CONFIG = 'disabled';
    const c = AsterConfig.fromEnv();
    expect(c.logFormat).toBe('json');
    expect(c.healthPort).toBe(8080);
    expect(isEnrollmentEnabled(c)).toBe(false);
    expect(c.relayMode).toBe('custom');
    expect(c.relayUrls).toEqual(['https://r1.example', 'https://r2.example']);
    expect(c.secretKey).toBe(KEY_A);
    expect(c.secretKeyFile).toBe('/tmp/node.key');
    expect(c.transportReceiveWindow).toBe(10_000_000_000);
    expect(c.portmapper).toBe(false);
  });

  it('rejects malformed values instead of guessing', () => {
    process.env.ASTER_ENABLE_HOOKS = 'maybe';
    expect(() => AsterConfig.fromEnv()).toThrow(/ASTER_ENABLE_HOOKS/);
  });

  it('refuses a retired consumer-admission variable, naming the replacement', () => {
    process.env.ASTER_ALLOW_ALL_CONSUMERS = 'false';
    expect(() => AsterConfig.fromEnv()).toThrow(/ASTER_ALLOW_ALL_CONSUMERS.*enrollment_credential/);
  });
});

describe('AsterConfig.fromFile', () => {
  it('parses aster.toml and overlays env', () => {
    const file = join(dir, 'aster.toml');
    writeFileSync(file, `
[trust]
root_pubkey = "${ROOT}"
enrollment_credential = "${join(dir, 'node.cred')}"
next_root_commitment = "${'cd'.repeat(32)}"
root_seq = 2
root_rotations = ["s1", "s2"]

[enrollment]
audience = "mission-control"
fleet_namespace = "${'ef'.repeat(32)}"
state_path = "${join(dir, 'enrollment.json')}"
replicas = ["${'01'.repeat(32)}"]
policy = { all_of = [{ scope = "ops" }, "tolerate_stale_policy"] }
renew = false

[enrollment.limits]
handshake_timeout_ms = 2500
sync_timeout_ms = 9000

[enrollment.roles]
scopes_as_roles = true
include_attributes = false

[enrollment.roles.scopes]
fleet = ["ops.status", "ops.logs"]

[enrollment.issuer]
delegation_file = "${join(dir, 'issuer.chain')}"
lifetime = "2h"

[storage]
path = "${join(dir, 'store')}"
gc_interval_ms = 60000

[network]
relay_mode = "disabled"
bind_addr = "127.0.0.1:0"
hook_timeout_ms = 10000
hook_failure_mode = "fail_open"
secret_key_file = "${join(dir, 'node.key')}"
transport_stream_receive_window = 4294967296
transport_initial_mtu = 1400

[logging]
format = "json"
level = "debug"

[health]
port = 9090
host = "0.0.0.0"
`);
    process.env.ASTER_LOG_LEVEL = 'error';
    const c = AsterConfig.fromFile(file);
    expect(c.rootPubkey).toBe(ROOT);
    expect(c.enrollmentCredentialFile).toBe(join(dir, 'node.cred'));
    expect(c.nextRootCommitment).toBe('cd'.repeat(32));
    expect(c.rootSeq).toBe(2);
    expect(c.rootRotations).toEqual(['s1', 's2']);
    expect(isEnrollmentEnabled(c)).toBe(true);
    const e = c.enrollment!;
    expect(e.audience).toBe('mission-control');
    expect(e.fleetNamespace).toBe('ef'.repeat(32));
    expect(e.statePath).toBe(join(dir, 'enrollment.json'));
    expect(e.replicas).toEqual(['01'.repeat(32)]);
    expect(e.policy).toEqual({ all_of: [{ scope: 'ops' }, 'tolerate_stale_policy'] });
    expect(e.renew).toBe(false);
    expect(e.limits?.handshakeTimeoutMs).toBe(2500);
    expect(e.limits?.syncTimeoutMs).toBe(9000);
    expect(e.roles?.scopesAsRoles).toBe(true);
    expect(e.roles?.includeAttributes).toBe(false);
    expect(e.roles?.scopes).toEqual({ fleet: ['ops.status', 'ops.logs'] });
    expect(e.issuerDelegationFile).toBe(join(dir, 'issuer.chain'));
    expect(e.issuerLifetimeSecs).toBe(7200);
    expect(c.storagePath).toBe(join(dir, 'store'));
    expect(c.gcIntervalMs).toBe(60000);
    expect(c.relayMode).toBe('disabled');
    expect(c.bindAddr).toBe('127.0.0.1:0');
    expect(c.hookTimeoutMs).toBe(10000);
    expect(c.hookFailureMode).toBe('fail_open');
    expect(c.secretKeyFile).toBe(join(dir, 'node.key'));
    expect(c.transportStreamReceiveWindow).toBe(4294967296);
    expect(c.transportInitialMtu).toBe(1400);
    expect(c.logFormat).toBe('json');
    expect(c.logLevel).toBe('error'); // env wins over the file
    expect(c.healthPort).toBe(9090);
    expect(c.healthHost).toBe('0.0.0.0');
  });

  it('reports a missing or malformed file', () => {
    expect(() => AsterConfig.fromFile(join(dir, 'nope.toml'))).toThrow();
    const bad = join(dir, 'bad.toml');
    writeFileSync(bad, '[network]\nrelay_mode = \n');
    expect(() => AsterConfig.fromFile(bad)).toThrow(/invalid TOML/);
    const retired = join(dir, 'retired.toml');
    writeFileSync(retired, '[trust]\nallow_all_consumers = false\n');
    expect(() => AsterConfig.fromFile(retired)).toThrow(/\[trust\] allow_all_consumers/);
  });
});

describe('AsterConfig round trip', () => {
  it('resolve(fromEnv()) is a fixed point', () => {
    process.env.ASTER_RELAY_MODE = 'staging';
    process.env.ASTER_GC_INTERVAL_MS = '1500';
    const env = AsterConfig.fromEnv();
    expect(AsterConfig.resolve(env)).toEqual(env);
  });

  it('round-trips every knob set in code', () => {
    const input: AsterConfig = {
      rootPubkey: Buffer.from(ROOT, 'hex'),
      secretKey: new Uint8Array(32).fill(0x11),
      secretKeyFile: join(dir, 'k'),
      storagePath: join(dir, 's'),
      gcIntervalMs: 250,
      relayUrls: ['https://relay.example'],
      bindAddr: '0.0.0.0:9000',
      localDiscovery: true,
      enableMonitoring: true,
      enableHooks: true,
      hookTimeoutMs: 1234,
      hookFailureMode: 'fail_open',
      clearIpTransports: true,
      clearRelayTransports: false,
      portmapper: true,
      proxyUrl: 'http://proxy:8080',
      proxyFromEnv: true,
      transportMaxConcurrentBidiStreams: 1000,
      transportMaxConcurrentUniStreams: 10,
      transportStreamReceiveWindow: 2n ** 40n,
      transportReceiveWindow: 5_000_000_000,
      transportSendWindow: 2n ** 60n,
      transportMaxIdleTimeoutMs: 30_000,
      transportKeepAliveIntervalMs: 5_000,
      transportInitialMtu: 1200,
      transportDatagramReceiveBufferSize: 65536,
      transportDatagramSendBufferSize: 65536,
      transportSendFairness: false,
      transportEnableSegmentationOffload: true,
      alpns: ['my/1'],
      logFormat: 'json',
      logLevel: 'debug',
      logMask: false,
      healthPort: 7000,
      healthHost: '0.0.0.0',
      healthEnabled: true,
    };
    const out = AsterConfig.resolve(input);
    expect(out.rootPubkey).toBe(ROOT);
    expect(out.secretKey).toBe(KEY_A);
    expect(out.relayMode).toBe('custom');
    expect(out.relayUrls).toEqual(['https://relay.example']);
    // Safe integers come back as numbers, larger ones as bigint.
    expect(out.transportStreamReceiveWindow).toBe(2 ** 40);
    expect(out.transportReceiveWindow).toBe(5_000_000_000);
    expect(out.transportSendWindow).toBe(2n ** 60n);
    expect(out.alpns?.map((a) => Buffer.from(a).toString())).toEqual(['my/1']);
    for (const key of [
      'secretKeyFile', 'storagePath', 'gcIntervalMs', 'bindAddr', 'localDiscovery',
      'enableMonitoring', 'enableHooks', 'hookTimeoutMs', 'hookFailureMode',
      'clearIpTransports', 'clearRelayTransports', 'portmapper', 'proxyUrl', 'proxyFromEnv',
      'transportMaxConcurrentBidiStreams', 'transportMaxConcurrentUniStreams',
      'transportMaxIdleTimeoutMs', 'transportKeepAliveIntervalMs', 'transportInitialMtu',
      'transportDatagramReceiveBufferSize', 'transportDatagramSendBufferSize',
      'transportSendFairness', 'transportEnableSegmentationOffload', 'logFormat',
      'logLevel', 'logMask', 'healthPort', 'healthHost', 'healthEnabled',
    ] as const) {
      expect(out[key], key).toEqual(input[key]);
    }
    // And the resolved form is itself stable.
    expect(AsterConfig.resolve(out)).toEqual(out);
  });

  it('validates knobs in Rust', () => {
    expect(() => AsterConfig.resolve({ hookTimeoutMs: -1 })).toThrow(/non-negative integer/);
    expect(() => AsterConfig.resolve({ transportSendWindow: 1.5 })).toThrow(/non-negative integer/);
    expect(() => AsterConfig.resolve({ transportInitialMtu: 70000 })).toThrow(/16 bits/);
    expect(() => AsterConfig.resolve({ hookFailureMode: 'sometimes' as any })).toThrow(/hookFailureMode/);
    expect(() => AsterConfig.resolve({ secretKey: 'zz' })).toThrow(/secretKey/);
    expect(() => AsterConfig.resolve({ relayMode: 'disabled', relayUrls: ['https://r'] }))
      .toThrow(/relayUrls/);
    // `{ ...fromEnv(), relayUrls }` (mode "default") selects custom relays.
    expect(AsterConfig.resolve({ relayMode: 'default', relayUrls: ['https://r'] }).relayMode)
      .toBe('custom');
  });
});

describe('blobZstdLevel and reuseLastPort', () => {
  it('default to level 3 and port reuse on', () => {
    const c = AsterConfig.fromEnv();
    expect(c.blobZstdLevel).toBe(3);
    expect(c.reuseLastPort).toBe(true);
  });

  it('are read from ASTER_BLOB_ZSTD_LEVEL / ASTER_REUSE_LAST_PORT', () => {
    process.env.ASTER_BLOB_ZSTD_LEVEL = 'off';
    process.env.ASTER_REUSE_LAST_PORT = 'false';
    const c = AsterConfig.fromEnv();
    expect(c.blobZstdLevel).toBe(0);
    expect(c.reuseLastPort).toBe(false);
    process.env.ASTER_BLOB_ZSTD_LEVEL = '7';
    expect(AsterConfig.fromEnv().blobZstdLevel).toBe(7);
  });

  it('are read from [storage] blob_zstd_level / [network] reuse_last_port', () => {
    const path = join(dir, 'aster.toml');
    writeFileSync(path, '[storage]\nblob_zstd_level = 9\n\n[network]\nreuse_last_port = false\n');
    const c = AsterConfig.fromFile(path);
    expect(c.blobZstdLevel).toBe(9);
    expect(c.reuseLastPort).toBe(false);
  });

  it('turn off in code and clamp like Rust', () => {
    const off = AsterConfig.resolve({ blobZstdLevel: 0, reuseLastPort: false });
    expect(off.blobZstdLevel).toBe(0);
    expect(off.reuseLastPort).toBe(false);
    expect(AsterConfig.resolve({ blobZstdLevel: 40 }).blobZstdLevel).toBe(19);
    expect(AsterConfig.resolve({}).blobZstdLevel).toBe(3);
  });
});

describe('enrollment configuration', () => {
  it('reads the ASTER_ENROLLMENT_* variables with the Rust names', () => {
    process.env.ASTER_ENROLLMENT_AUDIENCE = 'mc';
    process.env.ASTER_ENROLLMENT_FLEET_NAMESPACE = 'ab'.repeat(32);
    process.env.ASTER_ENROLLMENT_POLICY = '{"any_of":[{"tag":"edge"},"require_registration"]}';
    process.env.ASTER_ENROLLMENT_ROLE_ATTRIBUTE = 'team.role';
    process.env.ASTER_ENROLLMENT_HANDSHAKE_TIMEOUT_MS = '1500';
    process.env.ASTER_ENROLLMENT_CREDENTIAL = '/etc/aster/node.cred';
    process.env.ASTER_ENROLLMENT_REPLICAS = `${'01'.repeat(32)}, ${'02'.repeat(32)}`;
    process.env.ASTER_ROOT_ROTATIONS = 'a,b';
    process.env.ASTER_ROOT_SEQ = '2';
    const c = AsterConfig.fromEnv();
    expect(isEnrollmentEnabled(c)).toBe(true);
    expect(c.enrollmentCredentialFile).toBe('/etc/aster/node.cred');
    expect(c.rootRotations).toEqual(['a', 'b']);
    expect(c.rootSeq).toBe(2);
    expect(c.enrollment?.audience).toBe('mc');
    expect(c.enrollment?.policy).toEqual({ any_of: [{ tag: 'edge' }, 'require_registration'] });
    expect(c.enrollment?.roles?.attribute).toBe('team.role');
    expect(c.enrollment?.limits?.handshakeTimeoutMs).toBe(1500);
    expect(c.enrollment?.replicas).toEqual(['01'.repeat(32), '02'.repeat(32)]);
  });

  it('enablement follows the Rust rule: the root alone does not enable it', () => {
    expect(isEnrollmentEnabled({ rootPubkey: ROOT })).toBe(false);
    expect(isEnrollmentEnabled({ enrollment: { audience: 'mc' } })).toBe(true);
    expect(isEnrollmentEnabled({ enrollment: { fleetNamespace: 'ab'.repeat(32) } })).toBe(true);
    expect(isEnrollmentEnabled({ enrollmentCredentialChain: 'x' })).toBe(true);
    expect(isEnrollmentEnabled({ enrollmentCredentialFile: '/x.cred' })).toBe(true);
    expect(isEnrollmentEnabled({ enrollment: { audience: 'mc', enabled: false } })).toBe(false);
    expect(isEnrollmentEnabled({ enrollment: { enabled: true } })).toBe(true);
  });

  it('round-trips the section set in code', () => {
    const input: AsterConfig = {
      enrollmentCredentialChain: 'chain',
      nextRootCommitment: 'cd'.repeat(32),
      rootSeq: 1,
      rootRotations: ['r1'],
      enrollment: {
        audience: 'mc',
        fleetNamespace: 'ab'.repeat(32),
        statePath: join(dir, 'e.json'),
        replicas: ['01'.repeat(32)],
        policy: { all_of: [{ attribute_equals: { key: 'k', value: { type: 'int', value: 3 } } }, 'tolerate_stale_policy'] },
        requireSynchronizedClock: true,
        activationLeewaySecs: 30,
        limits: { stepTimeoutMs: 100, maxAdmittedPeers: 10 },
        roles: { attribute: '', scopes: { a: ['r'] }, tags: { t: ['x', 'y'] } },
        issuerDelegationFile: join(dir, 'issuer.chain'),
        issuerLifetimeSecs: 600,
        renewFrom: '01'.repeat(32),
        renew: true,
        synchronize: false,
      },
    };
    const out = AsterConfig.resolve(input);
    expect(out.enrollmentCredentialChain).toBe('chain');
    expect(out.nextRootCommitment).toBe('cd'.repeat(32));
    expect(out.rootSeq).toBe(1);
    expect(out.rootRotations).toEqual(['r1']);
    const e = out.enrollment!;
    expect(e.policy).toEqual(input.enrollment!.policy);
    expect(e.requireSynchronizedClock).toBe(true);
    expect(e.activationLeewaySecs).toBe(30);
    expect(e.limits).toMatchObject({ stepTimeoutMs: 100, maxAdmittedPeers: 10 });
    expect(e.roles).toEqual({
      attribute: '', scopesAsRoles: false, scopes: { a: ['r'] }, tags: { t: ['x', 'y'] }, includeAttributes: true,
    });
    expect(e.issuerLifetimeSecs).toBe(600);
    expect(e.renewFrom).toBe('01'.repeat(32));
    expect(e.renew).toBe(true);
    expect(e.synchronize).toBe(false);
    expect(AsterConfig.resolve(out)).toEqual(out);
  });

  it('mergeConfig overlays the enrollment section field by field', () => {
    const base: AsterConfig = {
      logLevel: 'info',
      enrollment: {
        audience: 'mc', fleetNamespace: 'ab'.repeat(32), limits: { stepTimeoutMs: 5 }, roles: { attribute: 'a' },
      },
    };
    const merged = mergeConfig(base, { enrollment: { statePath: '/s', limits: { syncTimeoutMs: 9 } } });
    expect(merged.enrollment).toEqual({
      audience: 'mc', fleetNamespace: 'ab'.repeat(32), statePath: '/s',
      limits: { stepTimeoutMs: 5, syncTimeoutMs: 9 }, roles: { attribute: 'a' },
    });
    expect(mergeConfig(base, { logLevel: 'debug' }).enrollment).toBe(base.enrollment);
  });
});

describe('resolveRootPubkey', () => {
  it('reads hex or JSON key files and checks them against the inline key', () => {
    const file = join(dir, 'root.json');
    writeFileSync(file, JSON.stringify({ public_key: ROOT }));
    expect(resolveRootPubkey({ rootPubkeyFile: file })).toBe(ROOT);
    expect(resolveRootPubkey({ rootPubkey: ROOT, rootPubkeyFile: file })).toBe(ROOT);
    expect(() => resolveRootPubkey({ rootPubkey: 'cd'.repeat(32), rootPubkeyFile: file }))
      .toThrow(/one deployment has one/);
    expect(resolveRootPubkey({ rootPubkeyFile: join(dir, 'missing') })).toBeUndefined();
  });
});

describe('loadIdentity (.aster-identity credential file)', () => {
  it('returns null for a missing file', () => {
    expect(loadIdentity('/nonexistent/.aster-identity')).toBeNull();
  });

  it('loads the node key (base64 or hex) and the first peer', () => {
    const file = join(dir, '.aster-identity');
    writeFileSync(file, `
[node]
secret_key = "${Buffer.alloc(32, 0x11).toString('base64')}"

[[peers]]
name = "producer1"
role = "producer"
root_pubkey = "${ROOT}"
expires_at = 1900000000

[peers.attributes]
team = "a"

[published_services.Echo]
producer_token = "tok"
`);
    const id = loadIdentity(file)!;
    expect(id.path).toBe(file);
    expect(id.secretKey).toBe(KEY_A);
    expect(id.peer?.name).toBe('producer1');
    expect(id.peer?.role).toBe('producer');
    expect(id.peer?.rootPubkey).toBe(ROOT);
    expect(id.peer?.expiresAt).toBe(1900000000);
    expect(id.peer?.attributes).toEqual({
      team: 'a',
      'aster.role': 'producer',
      'aster.name': 'producer1',
    });
    expect(Object.keys(getProducerTokens(id.peer!))).toEqual(['Echo']);
  });

  it('selects a peer by name or role', () => {
    const file = join(dir, 'id.toml');
    writeFileSync(file, `
[node]
secret_key = "${KEY_A}"

[[peers]]
name = "prod1"
role = "producer"

[[peers]]
name = "cons1"
role = "consumer"
`);
    expect(loadIdentity(file, undefined, 'consumer')!.peer?.name).toBe('cons1');
    expect(loadIdentity(file, 'prod1', 'consumer')!.peer?.name).toBe('prod1');
    expect(loadIdentity(file)!.peer?.name).toBe('prod1');
  });

  it('throws on a malformed file instead of returning null', () => {
    const file = join(dir, 'bad');
    writeFileSync(file, '[node]\nsecret_key = "not-a-key"\n');
    expect(() => loadIdentity(file)).toThrow();
  });
});
