/**
 * Gate 0 policy (Rust `GatePolicy` / `PeerAdmissionStore` via NAPI), the
 * low-level escape hatch. `AsterServer` has no gate: Enrollment owns Gate 0
 * when configured, and the node is open otherwise.
 */

import { describe, expect, it } from 'vitest';

import {
  AsterServer,
  DELEGATED_ADMISSION_ALPN,
  GatePolicy,
  PeerAdmissionStore,
  gateAdmitter,
} from '@aster-rpc/aster';
import { PeerAttributeStore, createPeerAdmission } from '../../../bindings/typescript/packages/aster/src/peer-store.js';

const PEER = 'aa'.repeat(32);
const OTHER = 'bb'.repeat(32);
const RPC = 'aster/1';

describe('GatePolicy', () => {
  it('protected: admission ALPNs are open, others need admission', () => {
    const gate = new GatePolicy();
    expect(gate.mode).toBe('protected');
    expect(gate.shouldAllow(PEER, DELEGATED_ADMISSION_ALPN).allow).toBe(true);
    expect(gate.shouldAllow(PEER, RPC)).toEqual({
      allow: false,
      errorCode: 403,
      reason: 'peer not admitted',
    });
    expect(gate.admissionAlpns).toContain('aster.admission');
  });

  it('admit / revoke', () => {
    const gate = GatePolicy.protected();
    expect(gate.admit(PEER)).toBe(false);
    expect(gate.admit(PEER)).toBe(true); // replaced
    expect(gate.isAdmitted(PEER)).toBe(true);
    expect(gate.isAdmitted(OTHER)).toBe(false);
    expect(gate.shouldAllow(PEER, RPC).allow).toBe(true);
    expect(gate.shouldAllow(PEER.toUpperCase(), RPC).allow).toBe(true);
    expect(gate.revoke(PEER)).toBe(true);
    expect(gate.revoke(PEER)).toBe(false);
    expect(gate.shouldAllow(PEER, RPC).allow).toBe(false);
  });

  it('open_dev allows every peer; mode can change in place', () => {
    const gate = GatePolicy.openDev();
    expect(gate.mode).toBe('open_dev');
    expect(gate.shouldAllow(PEER, RPC).allow).toBe(true);
    gate.mode = 'protected';
    expect(gate.shouldAllow(PEER, RPC).allow).toBe(false);
    expect(() => {
      gate.mode = 'nope' as any;
    }).toThrow(/trust mode/);
  });

  it('custom reject and admission ALPNs', () => {
    const gate = new GatePolicy({ admissionAlpns: ['custom.admit'], rejectCode: 451, rejectReason: 'go away' });
    expect(gate.shouldAllow(PEER, 'custom.admit').allow).toBe(true);
    expect(gate.shouldAllow(PEER, DELEGATED_ADMISSION_ALPN)).toEqual({ allow: false, errorCode: 451, reason: 'go away' });
    gate.setReject(499, 'later');
    gate.admissionAlpns = [];
    expect(gate.shouldAllow(PEER, 'custom.admit')).toEqual({ allow: false, errorCode: 499, reason: 'later' });
  });

  it('rejects malformed peer ids', () => {
    const gate = new GatePolicy();
    expect(() => gate.admit('not-a-node-id')).toThrow(/64-character hex/);
    expect(() => gate.shouldAllow('abc', RPC)).toThrow(/64-character hex/);
  });

  it('shares its store; expiry and attributes are enforced in Rust', () => {
    const store = new PeerAdmissionStore();
    const gate = new GatePolicy({ store });
    store.admit(PEER, { attributes: { 'aster.role': 'reader' }, source: 'credential', ttlMs: 60_000 });
    expect(gate.shouldAllow(PEER, RPC).allow).toBe(true);
    const rec = gate.store.get(PEER)!;
    expect(rec.attributes).toEqual({ 'aster.role': 'reader' });
    expect(rec.source).toBe('credential');
    expect(rec.expiresAt!.getTime()).toBeGreaterThan(Date.now());

    store.admit(OTHER, { expiresAt: new Date(Date.now() - 1000), attributes: new Map([['k', 'v']]) });
    expect(gate.isAdmitted(OTHER)).toBe(false);
    expect(store.attributes(OTHER)).toEqual({});
    expect(store.size).toBe(2);
    expect(store.pruneExpired()).toBe(1);
    expect(store.size).toBe(1);
  });
});

describe('credential-handler adapters over the Rust store', () => {
  it('PeerAttributeStore admissions open the gate, with the TTL cap and attributes', () => {
    const gate = new GatePolicy();
    const peers = new PeerAttributeStore(gate.store);
    const admitter = gateAdmitter(gate);
    peers.admit(createPeerAdmission({
      endpointId: PEER,
      attributes: new Map([['aster.role', 'writer']]),
      expiresAt: Date.now() / 1000 + 3600,
      admissionPath: 'aster.admission',
    }));
    // addPeer after a stored admission must not clobber it.
    admitter.addPeer(PEER);
    expect(gate.shouldAllow(PEER, RPC).allow).toBe(true);
    expect(peers.getAttributes(PEER).get('aster.role')).toBe('writer');
    expect(peers.getAttributes('garbage').size).toBe(0);
    expect(peers.get(PEER)!.expiresAt).toBeGreaterThan(Date.now() / 1000);

    // Already-expired credential: not admitted.
    peers.admit(createPeerAdmission({ endpointId: OTHER, expiresAt: 1 }));
    expect(gate.isAdmitted(OTHER)).toBe(false);
    admitter.addPeer(OTHER);
    expect(gate.isAdmitted(OTHER)).toBe(true);
    admitter.removePeer(OTHER);
    expect(gate.isAdmitted(OTHER)).toBe(false);
  });
});

describe('AsterServer and the retired consumer admission', () => {
  it('has no gate and no allow-all switch: open exactly when enrollment is off', () => {
    const server = new AsterServer({ services: [] });
    expect((server as any).gate).toBeUndefined();
    expect(server.enrolled).toBe(false);
    expect('allowAllConsumers' in server.config).toBe(false);
    expect('allowAllProducers' in server.config).toBe(false);
    expect('enrollmentCredentialIid' in server.config).toBe(false);
  });
});
