/** `CallContext`'s gateway-identity accessors (no network; see integration/gateway-identity.test.ts). */
import { describe, expect, it } from 'vitest';
import { AMR_PASSWORD, AMR_WEBAUTHN, CallContext, RpcError, StatusCode } from '@aster-rpc/aster';

const forwarded = {
  principal: 'edge|alice',
  actor: 'edge',
  issuer: 'edge',
  subject: 'alice',
  roles: ['notes.read'],
  authMethods: [AMR_WEBAUTHN],
  jti: 'j1',
  expiresAt: 1,
};

describe('CallContext gateway identity', () => {
  it('a call made by the peer as itself has no actor or sign-in method', () => {
    const ctx = new CallContext({ service: 'S', method: 'm', peer: 'edge' });
    expect(ctx.actor).toBeUndefined();
    expect(ctx.onBehalfOf).toBeUndefined();
    expect(ctx.authMethods).toEqual([]);
    let err: unknown;
    try {
      ctx.requireAuthMethod(AMR_WEBAUTHN);
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(RpcError);
    expect((err as RpcError).code).toBe(StatusCode.PERMISSION_DENIED);
  });

  it('a forwarded call names the gateway and the sign-in', () => {
    const ctx = new CallContext({ service: 'S', method: 'm', peer: 'edge', principal: 'edge|alice', onBehalfOf: forwarded });
    expect(ctx.actor).toBe('edge');
    expect(ctx.principal).toBe('edge|alice');
    expect(ctx.authMethods).toEqual([AMR_WEBAUTHN]);
    expect(() => ctx.requireAuthMethod(AMR_WEBAUTHN)).not.toThrow();
    expect(() => ctx.requireAuthMethod(AMR_PASSWORD)).toThrow(/sign-in with pwd/);
  });

  it('node properties default to empty and read typed attributes', () => {
    expect(new CallContext({ service: 'S', method: 'm' }).nodeProperties).toMatchObject({ generation: 0, name: '' });
    const ctx = new CallContext({
      service: 'S',
      method: 'm',
      nodeProperties: { generation: 2, name: 'n', tags: ['eu'], attributes: { tier: { type: 'int', value: 3 } } },
    });
    expect(ctx.nodeAttribute('tier')).toEqual({ type: 'int', value: 3 });
    expect(ctx.nodeAttribute('missing')).toBeUndefined();
    expect(ctx.nodeAttribute('toString')).toBeUndefined();
  });
});
