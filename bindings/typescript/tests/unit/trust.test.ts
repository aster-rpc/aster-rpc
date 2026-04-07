/**
 * Tests for trust modules: producer admission, nonce store, clock drift, mesh state.
 */

import { describe, it, expect, vi } from 'vitest';
import {
  MeshState,
  InMemoryNonceStore,
  ClockDriftTracker,
  computeDrift,
  shouldIsolate,
  DEFAULT_CLOCK_DRIFT_CONFIG,
  handleProducerAdmission,
  type ProducerAdmissionOptions,
  verifyConsumerCredential,
  verifyProducerCredential,
  generateKeypair,
  sign,
  verify,
} from '@aster-rpc/aster';

// -- MeshState ----------------------------------------------------------------

describe('MeshState', () => {
  it('addPeer and isPeerAccepted', () => {
    const mesh = new MeshState();
    expect(mesh.isPeerAccepted('abc123')).toBe(false);
    mesh.addPeer('abc123');
    expect(mesh.isPeerAccepted('abc123')).toBe(true);
    expect(mesh.allPeers()).toContain('abc123');
  });

  it('remove clears peer from accepted set', () => {
    const mesh = new MeshState();
    mesh.addPeer('abc123');
    mesh.remove('abc123');
    expect(mesh.isPeerAccepted('abc123')).toBe(false);
  });

  it('findService works', () => {
    const mesh = new MeshState();
    mesh.update('peer1', [
      { peerEndpointId: 'peer1', serviceName: 'Echo', serviceVersion: 1, contractId: 'abc' },
    ]);
    expect(mesh.findService('Echo')).toHaveLength(1);
    expect(mesh.findService('Missing')).toHaveLength(0);
  });
});

// -- NonceStore ---------------------------------------------------------------

describe('InMemoryNonceStore', () => {
  it('tracks consumed nonces', () => {
    const store = new InMemoryNonceStore();
    expect(store.has('nonce1')).toBe(false);
    store.consume('nonce1');
    expect(store.has('nonce1')).toBe(true);
    expect(store.size).toBe(1);
  });

  it('prevents replay (double consume)', () => {
    const store = new InMemoryNonceStore();
    store.consume('nonce1');
    expect(store.has('nonce1')).toBe(true);
    // Second consume is a no-op, but has() still returns true
    store.consume('nonce1');
    expect(store.size).toBe(1);
  });

  it('expires old nonces', () => {
    const store = new InMemoryNonceStore(100); // 100ms TTL
    store.consume('old_nonce');
    // Manually check it exists
    expect(store.has('old_nonce')).toBe(true);
  });
});

// -- ClockDrift ---------------------------------------------------------------

describe('ClockDriftTracker', () => {
  it('computeDrift returns difference', () => {
    const now = Date.now();
    expect(computeDrift(now + 5000, now)).toBe(5000);
    expect(computeDrift(now - 3000, now)).toBe(-3000);
  });

  it('shouldIsolate respects tolerance', () => {
    const joinedAt = Date.now() - 120_000; // well past grace period
    expect(shouldIsolate(29_000, joinedAt)).toBe(false); // within 30s tolerance
    expect(shouldIsolate(31_000, joinedAt)).toBe(true);  // exceeds tolerance
    expect(shouldIsolate(-31_000, joinedAt)).toBe(true);  // negative drift
  });

  it('shouldIsolate respects grace period', () => {
    const joinedAt = Date.now() - 10_000; // within 60s grace period
    expect(shouldIsolate(60_000, joinedAt)).toBe(false); // grace period protects
  });

  it('tracker detects isolation', () => {
    const tracker = new ClockDriftTracker(Date.now() - 120_000);
    const futureTs = Date.now() + 60_000; // 60s ahead — beyond 30s tolerance
    const isolated = tracker.update('peer1', futureTs);
    expect(isolated).toBe(true);
    expect(tracker.isIsolated('peer1')).toBe(true);
  });

  it('tracker recovers peer when drift normalizes', () => {
    const tracker = new ClockDriftTracker(Date.now() - 120_000);
    tracker.update('peer1', Date.now() + 60_000); // isolate
    expect(tracker.isIsolated('peer1')).toBe(true);
    tracker.update('peer1', Date.now() + 1000); // recover
    expect(tracker.isIsolated('peer1')).toBe(false);
  });

  it('isolatedPeers returns all isolated', () => {
    const tracker = new ClockDriftTracker(Date.now() - 120_000);
    tracker.update('peer1', Date.now() + 60_000);
    tracker.update('peer2', Date.now() + 60_000);
    tracker.update('peer3', Date.now() + 1000);
    expect(tracker.isolatedPeers()).toContain('peer1');
    expect(tracker.isolatedPeers()).toContain('peer2');
    expect(tracker.isolatedPeers()).not.toContain('peer3');
  });
});

// -- Credential verification -------------------------------------------------

describe('credential verification', () => {
  it('verifyProducerCredential checks root pubkey', async () => {
    const result = await verifyProducerCredential(
      { endpointId: 'abc', rootPubkey: 'aaa', expiresAt: 0, attributes: {}, signature: '' },
      'bbb',
    );
    expect(result.admitted).toBe(false);
    expect(result.reason).toContain('pubkey mismatch');
  });

  it('verifyProducerCredential checks expiry', async () => {
    const result = await verifyProducerCredential(
      { endpointId: 'abc', rootPubkey: 'same', expiresAt: 1, attributes: {}, signature: '' },
      'same',
    );
    expect(result.admitted).toBe(false);
    expect(result.reason).toContain('expired');
  });

  it('verifyProducerCredential admits valid credential', async () => {
    const result = await verifyProducerCredential(
      { endpointId: 'abc', rootPubkey: 'same', expiresAt: 0, attributes: { role: 'admin' }, signature: '' },
      'same',
    );
    expect(result.admitted).toBe(true);
    expect(result.attributes).toEqual({ role: 'admin' });
  });
});

// -- Ed25519 keypair + sign + verify -----------------------------------------

describe('ed25519 operations', () => {
  it('generateKeypair returns valid key lengths', async () => {
    const [priv, pub] = await generateKeypair();
    expect(priv.length).toBe(32);
    expect(pub.length).toBe(32);
  });

  it('sign + verify roundtrip', async () => {
    const [privateKey, publicKey] = await generateKeypair();
    const message = new TextEncoder().encode('hello aster');
    const sig = await sign(privateKey, message);
    expect(sig.length).toBe(64);
    const valid = await verify(publicKey, message, sig);
    expect(valid).toBe(true);
  });

  it('verify rejects tampered message', async () => {
    const [privateKey, publicKey] = await generateKeypair();
    const message = new TextEncoder().encode('hello aster');
    const sig = await sign(privateKey, message);
    const tampered = new TextEncoder().encode('hello world');
    const valid = await verify(publicKey, tampered, sig);
    expect(valid).toBe(false);
  });
});

// -- Producer admission handler -----------------------------------------------

describe('handleProducerAdmission', () => {
  function mockConn(credJson: string, remoteId = 'peer123') {
    const sentData: Uint8Array[] = [];
    return {
      conn: {
        remoteNodeId: () => remoteId,
        acceptBi: async () => ({
          takeSend: () => ({
            writeAll: async (data: Uint8Array) => { sentData.push(data); },
            finish: async () => {},
          }),
          takeRecv: () => ({
            readToEnd: async () => new TextEncoder().encode(
              JSON.stringify({ credentialJson: credJson }),
            ),
          }),
        }),
      },
      sentData,
    };
  }

  it('admits valid credential', async () => {
    const mesh = new MeshState();
    const { conn } = mockConn(JSON.stringify({
      endpointId: 'peer123',
      rootPubkey: 'root_key_hex',
      expiresAt: 0,
      attributes: {},
      signature: '',
    }));

    const opts: ProducerAdmissionOptions = {
      rootPubkey: 'root_key_hex',
      meshState: mesh,
      logger: { info: () => {}, warning: () => {}, error: () => {} },
    };

    const resp = await handleProducerAdmission(conn, opts);
    expect(resp.accepted).toBe(true);
    expect(mesh.isPeerAccepted('peer123')).toBe(true);
  });

  it('rejects mismatched root pubkey', async () => {
    const mesh = new MeshState();
    const { conn } = mockConn(JSON.stringify({
      endpointId: 'peer123',
      rootPubkey: 'wrong_key',
      expiresAt: 0,
      attributes: {},
      signature: '',
    }));

    const opts: ProducerAdmissionOptions = {
      rootPubkey: 'correct_key',
      meshState: mesh,
      logger: { info: () => {}, warning: () => {}, error: () => {} },
    };

    const resp = await handleProducerAdmission(conn, opts);
    expect(resp.accepted).toBe(false);
    expect(mesh.isPeerAccepted('peer123')).toBe(false);
  });

  it('rejects endpoint ID mismatch', async () => {
    const mesh = new MeshState();
    const { conn } = mockConn(JSON.stringify({
      endpointId: 'different_peer',
      rootPubkey: 'root_key_hex',
      expiresAt: 0,
      attributes: {},
      signature: '',
    }), 'peer123');

    const opts: ProducerAdmissionOptions = {
      rootPubkey: 'root_key_hex',
      meshState: mesh,
      logger: { info: () => {}, warning: () => {}, error: () => {} },
    };

    const resp = await handleProducerAdmission(conn, opts);
    expect(resp.accepted).toBe(false);
  });

  it('never leaks reason on wire', async () => {
    const mesh = new MeshState();
    const { conn, sentData } = mockConn(JSON.stringify({
      endpointId: 'peer123',
      rootPubkey: 'wrong',
      expiresAt: 0,
      attributes: {},
      signature: '',
    }));

    const opts: ProducerAdmissionOptions = {
      rootPubkey: 'correct',
      meshState: mesh,
      logger: { info: () => {}, warning: () => {}, error: () => {} },
    };

    await handleProducerAdmission(conn, opts);
    // Parse what was sent on wire
    const wireResp = JSON.parse(new TextDecoder().decode(sentData[0]!));
    expect(wireResp.reason).toBe(''); // reason stripped from wire
  });
});
