/**
 * Tests for config: env loading, TOML parsing, identity file loading.
 */

import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { configFromEnv, configFromFile, loadIdentity } from '@aster-rpc/aster';
import { writeFileSync, unlinkSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

describe('configFromEnv', () => {
  const saved: Record<string, string | undefined> = {};

  beforeEach(() => {
    // Save existing env vars
    for (const key of ['ASTER_LOG_FORMAT', 'ASTER_LOG_LEVEL', 'ASTER_HEALTH_PORT', 'ASTER_ALLOW_ALL_CONSUMERS']) {
      saved[key] = process.env[key];
    }
  });

  afterEach(() => {
    // Restore env vars
    for (const [key, val] of Object.entries(saved)) {
      if (val === undefined) delete process.env[key];
      else process.env[key] = val;
    }
  });

  it('returns defaults when no env vars set', () => {
    delete process.env.ASTER_LOG_FORMAT;
    delete process.env.ASTER_HEALTH_PORT;
    const config = configFromEnv();
    expect(config.logFormat).toBe('text');
    expect(config.logLevel).toBe('info');
    expect(config.logMask).toBe(true);
    expect(config.allowAllConsumers).toBe(false);
    expect(config.healthHost).toBe('127.0.0.1');
  });

  it('reads env vars', () => {
    process.env.ASTER_LOG_FORMAT = 'json';
    process.env.ASTER_HEALTH_PORT = '8080';
    process.env.ASTER_ALLOW_ALL_CONSUMERS = 'true';
    const config = configFromEnv();
    expect(config.logFormat).toBe('json');
    expect(config.healthPort).toBe(8080);
    expect(config.allowAllConsumers).toBe(true);
  });
});

describe('configFromFile', () => {
  const tmpFile = join(tmpdir(), `aster-test-config-${Date.now()}.toml`);

  afterEach(() => {
    try { unlinkSync(tmpFile); } catch { /* ok */ }
  });

  it('parses TOML config', () => {
    writeFileSync(tmpFile, `
[trust]
allow_all_consumers = true
allow_all_producers = false

[network]
relay_mode = "full"
hook_timeout_ms = 10000

[logging]
format = "json"
level = "debug"

[health]
port = 9090
host = "0.0.0.0"
`);
    // Clear env vars that would override
    const saved = process.env.ASTER_LOG_FORMAT;
    delete process.env.ASTER_LOG_FORMAT;
    delete process.env.ASTER_LOG_LEVEL;
    delete process.env.ASTER_HEALTH_PORT;
    delete process.env.ASTER_HEALTH_HOST;
    delete process.env.ASTER_ALLOW_ALL_CONSUMERS;
    delete process.env.ASTER_ALLOW_ALL_PRODUCERS;

    const config = configFromFile(tmpFile);
    expect(config.allowAllConsumers).toBe(true);
    expect(config.allowAllProducers).toBe(false);
    expect(config.relayMode).toBe('full');
    expect(config.hookTimeoutMs).toBe(10000);
    expect(config.logFormat).toBe('json');
    expect(config.logLevel).toBe('debug');
    expect(config.healthPort).toBe(9090);
    expect(config.healthHost).toBe('0.0.0.0');

    if (saved) process.env.ASTER_LOG_FORMAT = saved;
  });
});

describe('loadIdentity', () => {
  const tmpFile = join(tmpdir(), `aster-test-identity-${Date.now()}.toml`);

  afterEach(() => {
    try { unlinkSync(tmpFile); } catch { /* ok */ }
  });

  it('returns null for missing file', () => {
    expect(loadIdentity('/nonexistent/.aster-identity')).toBeNull();
  });

  it('loads identity from TOML', () => {
    // Base64 of 32 zero bytes
    const b64Key = Buffer.alloc(32).toString('base64');
    writeFileSync(tmpFile, `
[node]
secret_key = "${b64Key}"

[[peers]]
name = "producer1"
role = "producer"
endpoint_addr = "abc123"
`);

    const result = loadIdentity(tmpFile);
    expect(result).not.toBeNull();
    expect(result!.secretKey.length).toBe(32);
    expect(result!.peer.name).toBe('producer1');
    expect(result!.peer.role).toBe('producer');
  });

  it('selects peer by role', () => {
    const b64Key = Buffer.alloc(32).toString('base64');
    writeFileSync(tmpFile, `
[node]
secret_key = "${b64Key}"

[[peers]]
name = "prod1"
role = "producer"

[[peers]]
name = "cons1"
role = "consumer"
`);

    const result = loadIdentity(tmpFile, undefined, 'consumer');
    expect(result).not.toBeNull();
    expect(result!.peer.name).toBe('cons1');
  });
});
