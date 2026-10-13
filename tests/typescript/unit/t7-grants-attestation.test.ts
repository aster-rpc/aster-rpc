/**
 * Plan T7 golden vectors — identity-derived HPKE keys, sealed grants and
 * lease rules, all through the NAPI wrappers of the `aster` crate:
 *
 * - conformance/vectors/grants.json (R5): AAD, open / open_invalid,
 *   namespace grants, the role rule, the grant key and reconcile decisions;
 * - core/tests/fixtures/fory_golden_1_3_0.txt: the byte-stable HPKE envelope,
 *   namespace capabilities and grant records Rust core pins.
 */

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import * as path from 'node:path';
import {
  GrantRecord,
  HpkeEnvelope,
  NamespaceCapability,
  checkFence,
  checkGrantRole,
  grantAad,
  grantKey,
  hpkeOpen,
  hpkePublicKeyFromPrivate,
  hpkeSeal,
  hpkeX25519PublicFromIdentity,
  hpkeX25519PublicFromNodeId,
  hpkeX25519SecretFromIdentity,
  leaseTransition,
  openGrant,
  openGrantWithKey,
  openNamespaceGrant,
  reconcileGrant,
  reconcileNamespaceGrant,
  sealGrant,
  sealGrantToKey,
  sealNamespaceGrant,
  SecretKey,
  type GrantContext,
} from '@aster-rpc/aster';

const ROOT = path.resolve(__dirname, '../../..');
const grants = JSON.parse(readFileSync(path.join(ROOT, 'conformance/vectors/grants.json'), 'utf8'));
const golden: Record<string, string> = Object.fromEntries(
  readFileSync(path.join(ROOT, 'core/tests/fixtures/fory_golden_1_3_0.txt'), 'utf8')
    .split('\n')
    .filter((l) => l && !l.startsWith('#'))
    .map((l) => l.split(' ') as [string, string]),
);

const hex = (b: Uint8Array): string => Buffer.from(b).toString('hex');
const fromHex = (h: string): Uint8Array => new Uint8Array(Buffer.from(h, 'hex'));

function ctxOf(c: any): GrantContext {
  return {
    app: c.app,
    granter: c.granter,
    recipient: c.recipient,
    resource: fromHex(c.resource_hex),
    path: c.path,
    role: c.role,
  };
}

describe('grants.json (R5 vectors)', () => {
  it.each(grants.aad.map((v: any) => [v.name, v]))('aad %s', (_n, v: any) => {
    expect(hex(grantAad(ctxOf(v.context)))).toBe(v.aad_hex);
  });

  it('grant_key', () => {
    for (const v of grants.grant_key) expect(grantKey(v.prefix, v.recipient)).toBe(v.key);
  });

  it('role_rule', () => {
    for (const v of grants.role_rule) {
      if (v.ok) expect(() => checkGrantRole(v.role, v.can_write)).not.toThrow();
      else expect(() => checkGrantRole(v.role, v.can_write)).toThrow(/role/);
    }
  });

  it.each(grants.open.map((v: any) => [v.name, v]))('open %s', (_n, v: any) => {
    const ctx = ctxOf(v.context);
    const env = fromHex(v.envelope_hex);
    const plain =
      v.scheme === 'identity'
        ? openGrant(v.recipient_key_hex, ctx, env)
        : openGrantWithKey(fromHex(v.recipient_key_hex), ctx, env);
    expect(hex(plain)).toBe(v.plaintext_hex);
    if (v.recipient_public_hex) {
      expect(hex(hpkePublicKeyFromPrivate(fromHex(v.recipient_key_hex)))).toBe(v.recipient_public_hex);
    }
  });

  it.each(grants.open_invalid.map((v: any) => [v.name, v]))('open_invalid %s', (_n, v: any) => {
    const ctx = ctxOf(v.context);
    const env = fromHex(v.envelope_hex);
    const open = () =>
      v.scheme === 'identity'
        ? openGrant(v.recipient_key_hex, ctx, env)
        : openGrantWithKey(fromHex(v.recipient_key_hex), ctx, env);
    expect(open).toThrow();
  });

  it.each(grants.namespace_open.map((v: any) => [v.name, v]))('namespace_open %s', (_n, v: any) => {
    const cap = openNamespaceGrant(v.recipient_secret_hex, ctxOf(v.context), fromHex(v.envelope_hex));
    expect(hex(cap.encodeFory())).toBe(v.capability_hex);
    expect(cap.canWrite).toBe(v.can_write);
  });

  it('namespace_open_invalid', () => {
    for (const v of grants.namespace_open_invalid) {
      expect(() => openNamespaceGrant(v.recipient_secret_hex, ctxOf(v.context), fromHex(v.envelope_hex))).toThrow();
    }
  });

  it.each(grants.reconcile.map((v: any) => [v.name, v]))('reconcile %s', (_n, v: any) => {
    const ctx = ctxOf(v.context);
    const record = v.record_hex == null ? null : fromHex(v.record_hex);
    const want = v.outcome;
    const got =
      v.payload === 'namespace'
        ? reconcileNamespaceGrant(v.recipient_secret_hex, ctx, record)
        : reconcileGrant(v.recipient_secret_hex, ctx, record);
    expect(got.kind).toBe(want.kind);
    expect(got.dropsCapability).toBe(want.kind === 'revoked' || want.kind === 'unopenable');
    if (got.kind === 'revoked') {
      expect(got.reason).toBe(want.reason);
      expect(got.role).toBe(want.role);
    } else if (got.kind === 'unopenable' && want.error_contains) {
      expect(got.error.message).toContain(want.error_contains);
    } else if (got.kind === 'active') {
      if (v.payload === 'namespace') {
        expect(hex((got.value as NamespaceCapability).encodeFory())).toBe(want.capability_hex);
      } else {
        expect(hex(got.value as Uint8Array)).toBe(want.plaintext_hex);
      }
    }
  });
});

describe('sealed grants round trip', () => {
  const rootSecret = SecretKey.generate();
  const recipientSecret = SecretKey.generate();
  const ctx: GrantContext = {
    app: 'ts/test-grant/v1',
    granter: SecretKey.nodeId(rootSecret),
    recipient: SecretKey.nodeId(recipientSecret),
    resource: new Uint8Array(32).fill(7),
    path: grantKey('/ts/v1', SecretKey.nodeId(recipientSecret)),
    role: 'write',
  };

  it('seal to the identity-derived key, open with the identity secret', () => {
    const env = sealGrant(ctx, new TextEncoder().encode('secret'));
    expect(new TextDecoder().decode(openGrant(recipientSecret, ctx, env))).toBe('secret');
    // Same keys through the explicit-key path.
    expect(hex(openGrantWithKey(hpkeX25519SecretFromIdentity(recipientSecret), ctx, env))).toBe(hex(new TextEncoder().encode('secret')));
    // Any context field change fails.
    expect(() => openGrant(recipientSecret, { ...ctx, role: 'read' }, env)).toThrow();
    expect(() => openGrant(rootSecret, ctx, env)).toThrow();
  });

  it('seal to an explicit key', () => {
    const recipientKey = hpkeX25519PublicFromNodeId(ctx.recipient);
    const env = sealGrantToKey(recipientKey, ctx, new Uint8Array([1, 2, 3]));
    expect([...openGrant(recipientSecret, ctx, env)]).toEqual([1, 2, 3]);
  });

  it('namespace grants enforce the role rule both ways', () => {
    const nsSecret = new Uint8Array(32).fill(9);
    const write = NamespaceCapability.write(nsSecret);
    const env = sealNamespaceGrant(ctx, write);
    const opened = openNamespaceGrant(recipientSecret, ctx, env);
    expect(opened.canWrite).toBe(true);
    expect(hex(opened.exposeSecret())).toBe(hex(nsSecret));
    expect(() => sealNamespaceGrant({ ...ctx, role: 'read' }, write)).toThrow(/role/);
  });

  it('records: active, revoked, reconcile', () => {
    const env = sealNamespaceGrant(ctx, NamespaceCapability.write(new Uint8Array(32).fill(3)));
    const active = GrantRecord.active('write', 1, env);
    const decoded = GrantRecord.decode(active.encode());
    expect(decoded.isActive).toBe(true);
    expect(hex(decoded.envelope)).toBe(hex(env));
    const outcome = reconcileNamespaceGrant(recipientSecret, ctx, active.encode());
    expect(outcome.kind).toBe('active');

    const revoked = GrantRecord.revoked('write', 'rotated');
    const r = reconcileNamespaceGrant(recipientSecret, ctx, revoked.encode());
    expect(r).toMatchObject({ kind: 'revoked', reason: 'rotated', role: 'write', dropsCapability: true });
    expect(reconcileGrant(recipientSecret, ctx, undefined)).toEqual({ kind: 'absent', dropsCapability: false });
    expect(() => GrantRecord.active('write', 1, new Uint8Array(0))).toThrow();
  });
});

describe('identity-derived HPKE keys', () => {
  it('public-from-identity agrees with public-from-private(secret-from-identity)', () => {
    const secret = SecretKey.generate();
    const id = SecretKey.nodeId(secret);
    const fromPublic = hpkeX25519PublicFromNodeId(id);
    expect(hex(hpkeX25519PublicFromIdentity(id))).toBe(hex(fromPublic));
    expect(hex(hpkePublicKeyFromPrivate(hpkeX25519SecretFromIdentity(fromHex(secret))))).toBe(hex(fromPublic));
    const env = hpkeSeal(fromPublic, new Uint8Array([9]), new Uint8Array([4, 5]));
    expect([...hpkeOpen(hpkeX25519SecretFromIdentity(secret), new Uint8Array([9]), env)]).toEqual([4, 5]);
  });

  it('rejects invalid identity keys', () => {
    expect(() => hpkeX25519PublicFromNodeId('zz')).toThrow();
    expect(() => hpkeX25519PublicFromIdentity(new Uint8Array(31))).toThrow();
    // The identity point is small-order.
    expect(() => hpkeX25519PublicFromIdentity('01' + '00'.repeat(31))).toThrow();
  });
});

describe('Fory golden fixtures (core byte stability)', () => {
  it('hpke envelope, namespace capabilities and grant records', () => {
    const env = HpkeEnvelope.decodeFory(fromHex(golden['hpke.envelope']));
    expect(hex(env.encappedKey)).toBe('33'.repeat(32));
    expect(hex(env.ciphertext)).toBe('44'.repeat(48));
    expect(hex(NamespaceCapability.read(new Uint8Array(32).fill(0x11)).encodeFory())).toBe(golden['namespace.read']);
    expect(hex(NamespaceCapability.write(new Uint8Array(32).fill(0x22)).encodeFory())).toBe(golden['namespace.write']);
    expect(hex(GrantRecord.active('write', 1, env.encodeFory()).encode())).toBe(golden['grant.active']);
    expect(hex(GrantRecord.revoked('read', 'rotated').encode())).toBe(golden['grant.revoked']);
    const revoked = GrantRecord.decode(fromHex(golden['grant.revoked']));
    expect(revoked).toMatchObject({ state: 'revoked', role: 'read', reason: 'rotated', contextVersion: 1 });
  });
});

describe('lease rules (pure)', () => {
  const free = { epoch: 0, held: false, ttlMs: 0 };

  it('transition: acquire, renew, release, revoke', () => {
    const a = leaseTransition(free, false, { kind: 'acquire', actor: 'n1', ttlMs: 1000 }, 'r');
    expect(a.kind).toBe('apply');
    if (a.kind !== 'apply') return;
    expect(a.next).toEqual({ epoch: 1, holder: 'n1', held: true, ttlMs: 1000 });
    expect(a.outcome).toEqual({ kind: 'granted', fence: { resource: 'r', epoch: 1, holder: 'n1' } });
    expect(a.resetGrant).toBe(true);

    expect(leaseTransition(a.next, false, { kind: 'acquire', actor: 'n2', ttlMs: 1000 }, 'r')).toEqual({
      kind: 'deny',
      denyReason: 'held_by_other',
    });
    const takeover = leaseTransition(a.next, true, { kind: 'acquire', actor: 'n2', ttlMs: 1000 }, 'r');
    expect(takeover.kind === 'apply' && takeover.next.epoch).toBe(2);
    expect(leaseTransition(a.next, true, { kind: 'renew', actor: 'n1', epoch: 1 }, 'r')).toEqual({
      kind: 'deny',
      denyReason: 'expired',
    });
    const rel = leaseTransition(a.next, false, { kind: 'release', actor: 'n1', epoch: 1 }, 'r');
    expect(rel.kind === 'apply' && rel.next).toEqual({ epoch: 2, held: false, ttlMs: 1000 });
    const rev = leaseTransition(a.next, false, { kind: 'revoke', actor: 'op' }, 'r');
    expect(rev.kind === 'apply' && rev.outcome.kind).toBe('revoked');
  });

  it('checkFence', () => {
    const row = { epoch: 3, holder: 'n1', held: true, ttlMs: 1000 };
    expect(checkFence(row, { resource: 'r', epoch: 3, holder: 'n1' }, 'n1')).toBeNull();
    expect(checkFence(row, { resource: 'r', epoch: 2, holder: 'n1' }, 'n1')).toMatchObject({
      kind: 'stale_epoch',
      carried: 2,
      current: 3,
    });
    expect(checkFence(row, { resource: 'r', epoch: 4, holder: 'n1' }, 'n1')?.kind).toBe('future_epoch');
    expect(checkFence(row, { resource: 'r', epoch: 3, holder: 'n1' }, 'n2')?.kind).toBe('not_holder');
    expect(checkFence({ ...row, held: false }, { resource: 'r', epoch: 3, holder: 'n1' }, 'n1')?.kind).toBe('not_held');
  });
});
