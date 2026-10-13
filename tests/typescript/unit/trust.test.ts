/**
 * Tests for the ed25519 helpers (the legacy credential / nonce modules were
 * retired with consumer admission).
 */

import { describe, it, expect } from 'vitest';
import {
  generateKeypair,
  sign,
  verify,
} from '@aster-rpc/aster';

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
