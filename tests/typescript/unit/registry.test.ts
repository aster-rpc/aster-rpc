/**
 * Tests for registry: keys, models, ACL, publisher, gossip.
 */

import { describe, it, expect } from 'vitest';
import {
  keySegment,
  decodeKeySegment,
  contractKey,
  manifestKey,
  versionKey,
  versionPrefix,
  parseVersionKey,
  channelKey,
  tagKey,
  leaseKey,
  leasePrefix,
  aclKey,
  configKey,
  REGISTRY_PREFIXES,
  HealthStatus,
  GossipEventType,
  isLeaseFresh,
  isLeaseRoutable,
  RegistryACL,
  RegistryPublisher,
  type EndpointLease,
} from '@aster-rpc/aster';

// -- Keys ---

describe('registry keys', () => {
  it('contractKey', () => {
    expect(contractKey('abc123')).toBe('contracts/abc123/');
  });
  it('manifestKey', () => {
    expect(manifestKey('abc123')).toBe('manifests/abc123/');
  });
  it('versionKey', () => {
    expect(versionKey('Echo', 2)).toBe('services/Echo/versions/v2/');
    expect(versionPrefix('Echo')).toBe('services/Echo/versions/');
  });
  it('channelKey', () => {
    expect(channelKey('Echo', 'stable')).toBe('services/Echo/channels/stable/');
  });
  it('tagKey', () => {
    expect(tagKey('Echo', 'latest')).toBe('services/Echo/tags/latest/');
  });
  it('leaseKey', () => {
    expect(leaseKey('Echo', 'cid', 'eid')).toBe('services/Echo/contracts/cid/endpoints/eid/');
  });
  it('leasePrefix', () => {
    expect(leasePrefix('Echo', 'cid')).toBe('services/Echo/contracts/cid/endpoints/');
  });
  it('aclKey', () => {
    expect(aclKey('writers')).toBe('_aster/acl/writers/');
  });
  it('configKey', () => {
    expect(configKey('version')).toBe('_aster/config/version/');
  });
  it('segments escape slash and percent', () => {
    expect(keySegment('a/b%c')).toBe('a%2Fb%25c');
    expect(decodeKeySegment('a%2Fb%25c')).toBe('a/b%c');
    expect(decodeKeySegment('%252F')).toBe('%2F');
    expect(channelKey('x', 'a/b')).toBe('services/x/channels/a%2Fb/');
    for (const bad of ['a/b', '%', '%2', '%41', '50%']) {
      expect(decodeKeySegment(bad)).toBeNull();
    }
  });
  it('version keys parse strictly', () => {
    expect(parseVersionKey(versionKey('a/b', 10))).toEqual(['a/b', 10]);
    expect(parseVersionKey(versionKey('svc', -3))).toEqual(['svc', -3]);
    for (const bad of [
      'services/svc/versions/v1',
      'services/svc/versions/v01/',
      'services/svc/versions/v+1/',
      'services/svc/versions/v-0/',
      'services/svc/versions/1/',
      'services/svc/versions/v1/x/',
      'services/a/b/versions/v1/',
      'services/svc/channels/v1/',
      'services/svc/versions/v2147483648/',
    ]) {
      expect(parseVersionKey(bad)).toBeNull();
    }
  });
  it('keys are prefix-free (iroh-docs supersedes extensions of a written key)', () => {
    const leaves = ['1', '10', 'stable', 'stable2', 'x/y', 'x', '', '%2F'];
    const keys = new Set<string>();
    for (const name of ['svc', 'svc2', 'a/b', 'a', '50%']) {
      for (const leaf of leaves) {
        [contractKey(leaf), manifestKey(leaf), aclKey(leaf), configKey(leaf)].forEach(k => keys.add(k));
        keys.add(channelKey(name, leaf));
        keys.add(tagKey(name, leaf));
        for (const eid of leaves) keys.add(leaseKey(name, leaf, eid));
      }
      for (const v of [1, 10, 100, -1]) keys.add(versionKey(name, v));
    }
    for (const a of keys) {
      for (const b of keys) {
        if (a !== b) expect(b.startsWith(a), `${a} prefixes ${b}`).toBe(false);
      }
    }
  });
  it('REGISTRY_PREFIXES has expected entries', () => {
    expect(REGISTRY_PREFIXES).toContain('contracts/');
    expect(REGISTRY_PREFIXES).toContain('_aster/');
  });
});

// -- Models ---

describe('registry models', () => {
  it('HealthStatus values', () => {
    expect(HealthStatus.STARTING).toBe('starting');
    expect(HealthStatus.READY).toBe('ready');
    expect(HealthStatus.DEGRADED).toBe('degraded');
    expect(HealthStatus.DRAINING).toBe('draining');
  });

  it('GossipEventType values', () => {
    expect(GossipEventType.CONTRACT_PUBLISHED).toBe(0);
    expect(GossipEventType.ENDPOINT_DOWN).toBe(3);
  });

  it('isLeaseFresh detects expired', () => {
    const lease: EndpointLease = {
      endpointId: 'e', contractId: 'c', service: 's', version: 1,
      leaseExpiresEpochMs: Date.now() - 60_000, leaseSeq: 0,
      alpn: 'aster/1', serializationModes: [], featureFlags: [],
      directAddrs: [], healthStatus: HealthStatus.READY, tags: [],
      updatedAtEpochMs: Date.now() - 60_000,
    };
    expect(isLeaseFresh(lease)).toBe(false);
  });

  it('isLeaseFresh accepts valid', () => {
    const lease: EndpointLease = {
      endpointId: 'e', contractId: 'c', service: 's', version: 1,
      leaseExpiresEpochMs: Date.now() + 30_000, leaseSeq: 0,
      alpn: 'aster/1', serializationModes: [], featureFlags: [],
      directAddrs: [], healthStatus: HealthStatus.READY, tags: [],
      updatedAtEpochMs: Date.now(),
    };
    expect(isLeaseFresh(lease)).toBe(true);
  });

  it('isLeaseRoutable requires ready + fresh', () => {
    const readyFresh: EndpointLease = {
      endpointId: 'e', contractId: 'c', service: 's', version: 1,
      leaseExpiresEpochMs: Date.now() + 30_000, leaseSeq: 0,
      alpn: 'aster/1', serializationModes: [], featureFlags: [],
      directAddrs: [], healthStatus: HealthStatus.READY, tags: [],
      updatedAtEpochMs: Date.now(),
    };
    expect(isLeaseRoutable(readyFresh)).toBe(true);

    const draining = { ...readyFresh, healthStatus: HealthStatus.DRAINING as any };
    expect(isLeaseRoutable(draining)).toBe(false);
  });
});

// -- ACL ---

describe('RegistryACL', () => {
  it('starts in open mode', () => {
    const acl = new RegistryACL();
    expect(acl.restricted).toBe(false);
    expect(acl.isTrustedWriter('anyone')).toBe(true);
  });

  it('switches to restricted after addWriter', async () => {
    const acl = new RegistryACL();
    await acl.addWriter('author1');
    expect(acl.restricted).toBe(true);
    expect(acl.isTrustedWriter('author1')).toBe(true);
    expect(acl.isTrustedWriter('other')).toBe(false);
  });

  it('filterTrusted in open mode passes all', () => {
    const acl = new RegistryACL();
    const entries = [{ authorId: 'a' }, { authorId: 'b' }];
    expect(acl.filterTrusted(entries)).toHaveLength(2);
  });

  it('filterTrusted in restricted mode filters', async () => {
    const acl = new RegistryACL();
    await acl.addWriter('trusted');
    const entries = [{ authorId: 'trusted' }, { authorId: 'untrusted' }];
    expect(acl.filterTrusted(entries)).toHaveLength(1);
    expect(acl.filterTrusted(entries)[0]!.authorId).toBe('trusted');
  });

  it('admins are trusted writers', async () => {
    const acl = new RegistryACL();
    await acl.addWriter('writer');
    acl.addAdmin('admin');
    expect(acl.isTrustedWriter('admin')).toBe(true);
  });
});

// -- Publisher ---

describe('RegistryPublisher', () => {
  it('publishContract writes to doc', async () => {
    const writes: [string, string][] = [];
    const mockDoc = {
      setBytes: async (_a: string, key: string, _v: Uint8Array) => {
        writes.push([key, new TextDecoder().decode(_v)]);
        return 'hash';
      },
    };

    const pub = new RegistryPublisher({
      doc: mockDoc as any,
      authorId: 'auth1',
      endpointId: 'ep1',
      logger: { info: () => {}, error: () => {} },
    });

    await pub.publishContract(
      { contractId: 'cid123', collectionHash: 'ch', publishedBy: 'auth1', publishedAtEpochMs: 0, collectionFormat: 'raw' },
      'Echo', 1,
    );

    expect(writes.some(([k]) => k === 'contracts/cid123/')).toBe(true);
    expect(writes.some(([k]) => k === 'services/Echo/versions/v1/')).toBe(true);
    pub.close();
  });
});
