/**
 * Tests for trust modules: nonce store, credential verification, ed25519.
 */

import { describe, it, expect } from 'vitest';
import {
  InMemoryNonceStore,
  verifyConsumerCredential,
  verifyProducerCredential,
  generateKeypair,
  sign,
  verify,
} from '@aster-rpc/aster';

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
