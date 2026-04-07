/**
 * Tests for registry: keys, models, ACL, publisher, gossip.
 */

import { describe, it, expect } from 'vitest';
import {
  contractKey,
  versionKey,
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
  RegistryClient,
  registryKey,
  RegistryPublisher,
  ConnectionMetrics,
  AdmissionMetrics,
  type EndpointLease,
} from '@aster-rpc/aster';

// -- Keys ---

describe('registry keys', () => {
  it('contractKey', () => {
    expect(contractKey('abc123')).toBe('contracts/abc123');
  });
  it('versionKey', () => {
    expect(versionKey('Echo', 2)).toBe('services/Echo/versions/v2');
  });
  it('channelKey', () => {
    expect(channelKey('Echo', 'stable')).toBe('services/Echo/channels/stable');
  });
  it('tagKey', () => {
    expect(tagKey('Echo', 'latest')).toBe('services/Echo/tags/latest');
  });
  it('leaseKey', () => {
    expect(leaseKey('Echo', 'cid', 'eid')).toBe('services/Echo/contracts/cid/endpoints/eid');
  });
  it('leasePrefix', () => {
    expect(leasePrefix('Echo', 'cid')).toBe('services/Echo/contracts/cid/endpoints/');
  });
  it('aclKey', () => {
    expect(aclKey('writers')).toBe('_aster/acl/writers');
  });
  it('configKey', () => {
    expect(configKey('version')).toBe('_aster/config/version');
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

// -- RegistryClient ---

describe('RegistryClient', () => {
  it('registryKey format', () => {
    expect(registryKey('Echo', 1)).toBe('contracts/Echo/v1');
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

    expect(writes.some(([k]) => k === 'contracts/cid123')).toBe(true);
    expect(writes.some(([k]) => k === 'services/Echo/versions/v1')).toBe(true);
    pub.close();
  });
});

// -- Metrics ---

describe('ConnectionMetrics', () => {
  it('tracks accept/reject/close', () => {
    const m = new ConnectionMetrics();
    m.onAccept();
    m.onAccept();
    m.onReject();
    m.onClose();
    const snap = m.snapshot();
    expect(snap.connections_accepted).toBe(2);
    expect(snap.connections_rejected).toBe(1);
    expect(snap.connections_closed).toBe(1);
    expect(snap.connections_active).toBe(1);
  });
});

describe('AdmissionMetrics', () => {
  it('tracks attempt/success/reject/error', () => {
    const m = new AdmissionMetrics();
    m.onAttempt();
    m.onAttempt();
    m.onSuccess();
    m.onReject();
    const snap = m.snapshot();
    expect(snap.admissions_attempted).toBe(2);
    expect(snap.admissions_succeeded).toBe(1);
    expect(snap.admissions_rejected).toBe(1);
    expect(snap.admissions_errored).toBe(0);
  });
});
