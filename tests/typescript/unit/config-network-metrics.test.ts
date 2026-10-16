/**
 * AsterConfig network settings (bind_addr_v6, advertise_addrs, relay CA,
 * discovery) and `[metrics]` peer labels: env, TOML and code give the same
 * Rust values (C ABI 7.3 / 7.4 parity, through NAPI).
 */

import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { AsterConfig, mergeConfig } from '@aster-rpc/aster';

const KEYS = [
  'ASTER_BIND_ADDR', 'ASTER_BIND_ADDR_V6', 'ASTER_ADVERTISE_ADDRS', 'ASTER_RELAY_CA_CERTS',
  'ASTER_RELAY_CA_ONLY', 'ASTER_DISCOVERY', 'ASTER_DISCOVERY_DNS_ORIGIN',
  'ASTER_DISCOVERY_PKARR_RELAYS', 'ASTER_METRICS_PEER_LABELS', 'ASTER_METRICS_PEER_LABEL_CAP',
];

let saved: Record<string, string | undefined> = {};
let dir: string;

beforeEach(() => {
  saved = {};
  for (const k of Object.keys(process.env).filter((k) => k.startsWith('ASTER_')).concat(KEYS)) {
    saved[k] = process.env[k];
    delete process.env[k];
  }
  dir = mkdtempSync(join(tmpdir(), 'aster-config-net-'));
});

afterEach(() => {
  for (const [k, v] of Object.entries(saved)) {
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
  rmSync(dir, { recursive: true, force: true });
});

const NETWORK = {
  bindAddr: '0.0.0.0:7842',
  bindAddrV6: '[::]:7842',
  advertiseAddrs: ['192.0.2.10:7842', '198.51.100.7:443'],
  relayCaCerts: ['/etc/aster/relay-ca.pem', '/etc/aster/relay-ca-2.pem'],
  relayCaOnly: true,
  discovery: 'custom' as const,
  discoveryDnsOrigin: 'dns.example.internal',
  discoveryPkarrRelays: ['https://dns.example.internal/pkarr'],
  metricsPeerLabels: true,
  metricsPeerLabelCap: 8,
};

function pick(c: AsterConfig) {
  return Object.fromEntries(Object.keys(NETWORK).map((k) => [k, (c as Record<string, unknown>)[k]]));
}

describe('network and metrics settings', () => {
  it('default to the Rust values', () => {
    const c = AsterConfig.fromEnv();
    expect(c.bindAddrV6).toBeUndefined();
    expect(c.advertiseAddrs).toEqual([]);
    expect(c.relayCaCerts).toEqual([]);
    expect(c.relayCaOnly).toBe(false);
    expect(c.discovery).toBe('default');
    expect(c.discoveryDnsOrigin).toBeUndefined();
    expect(c.discoveryPkarrRelays).toEqual([]);
    expect(c.metricsPeerLabels).toBe(false);
    expect(c.metricsPeerLabelCap).toBe(64);
  });

  it('env, TOML and code agree', () => {
    process.env.ASTER_BIND_ADDR = NETWORK.bindAddr;
    process.env.ASTER_BIND_ADDR_V6 = NETWORK.bindAddrV6;
    process.env.ASTER_ADVERTISE_ADDRS = NETWORK.advertiseAddrs.join(',');
    process.env.ASTER_RELAY_CA_CERTS = NETWORK.relayCaCerts.join(',');
    process.env.ASTER_RELAY_CA_ONLY = 'true';
    process.env.ASTER_DISCOVERY = 'custom';
    process.env.ASTER_DISCOVERY_DNS_ORIGIN = NETWORK.discoveryDnsOrigin;
    process.env.ASTER_DISCOVERY_PKARR_RELAYS = NETWORK.discoveryPkarrRelays.join(',');
    process.env.ASTER_METRICS_PEER_LABELS = 'true';
    process.env.ASTER_METRICS_PEER_LABEL_CAP = '8';
    const fromEnv = pick(AsterConfig.fromEnv());
    for (const k of KEYS) delete process.env[k];

    const toml = join(dir, 'aster.toml');
    writeFileSync(toml, `
[network]
bind_addr = "${NETWORK.bindAddr}"
bind_addr_v6 = "${NETWORK.bindAddrV6}"
advertise_addrs = ${JSON.stringify(NETWORK.advertiseAddrs)}
relay_ca_certs = ${JSON.stringify(NETWORK.relayCaCerts)}
relay_ca_only = true
discovery = "custom"
discovery_dns_origin = "${NETWORK.discoveryDnsOrigin}"
discovery_pkarr_relays = ${JSON.stringify(NETWORK.discoveryPkarrRelays)}

[metrics]
peer_labels = true
peer_label_cap = 8
`);
    const fromFile = pick(AsterConfig.fromFile(toml));
    const fromCode = pick(AsterConfig.resolve(NETWORK));

    expect(fromEnv).toEqual(NETWORK);
    expect(fromFile).toEqual(NETWORK);
    expect(fromCode).toEqual(NETWORK);
    // A resolved config is a fixed point.
    expect(pick(AsterConfig.resolve(AsterConfig.resolve(NETWORK)))).toEqual(NETWORK);
  });

  it('custom discovery servers select the custom mode, as the Rust loaders do', () => {
    process.env.ASTER_DISCOVERY_DNS_ORIGIN = 'dns.example.internal';
    expect(AsterConfig.fromEnv().discovery).toBe('custom');
    delete process.env.ASTER_DISCOVERY_DNS_ORIGIN;

    // `{ ...fromEnv(), discoveryPkarrRelays }`: the servers win over the
    // default mode the env load filled in.
    const merged = mergeConfig(AsterConfig.fromEnv(), { discoveryPkarrRelays: ['https://dns.example.internal/pkarr'] });
    expect(AsterConfig.resolve(merged).discovery).toBe('custom');
  });

  it('refuses contradictions and malformed values', () => {
    expect(() => AsterConfig.resolve({ discovery: 'none', discoveryDnsOrigin: 'dns.example.internal' }))
      .toThrow(/require discovery "custom"/);
    expect(() => AsterConfig.resolve({ discovery: 'sideways' as never })).toThrow(/discovery/);
    process.env.ASTER_DISCOVERY = 'local';
    process.env.ASTER_DISCOVERY_DNS_ORIGIN = 'dns.example.internal';
    expect(() => AsterConfig.fromEnv()).toThrow();
  });

  it('keeps the local and none modes', () => {
    expect(AsterConfig.resolve({ discovery: 'local' }).discovery).toBe('local');
    expect(AsterConfig.resolve({ discovery: 'none' }).discovery).toBe('none');
  });
});
