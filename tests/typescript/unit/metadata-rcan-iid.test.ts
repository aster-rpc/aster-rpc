/**
 * Tests for Metadata, RCAN validation, and IID (cloud identity).
 */

import { describe, it, expect } from 'vitest';
import {
  Metadata,
  Service,
  Rpc,
  ServerStream,
  WireType,
  WIRE_TYPE_KEY,
  WIRE_TYPE_FIELDS_KEY,
  evaluateCapability,
  extractCallerRoles,
  validateRcan,
  encodeRcan,
  decodeRcan,
  verifyIID,
  MockIIDBackend,
  getIIDBackend,
  ATTR_IID_PROVIDER,
  ATTR_IID_ACCOUNT,
  ATTR_IID_REGION,
  SERVICE_INFO_KEY,
  METHOD_INFO_KEY,
} from '@aster-rpc/aster';

// -- Metadata ----------------------------------------------------------------

describe('Metadata', () => {
  it('has wire type _aster/Metadata', () => {
    expect((Metadata as any)[WIRE_TYPE_KEY]).toBe('_aster/Metadata');
  });

  it('defaults to empty description', () => {
    const m = new Metadata();
    expect(m.description).toBe('');
  });

  it('accepts description in constructor', () => {
    const m = new Metadata({ description: 'Hello' });
    expect(m.description).toBe('Hello');
  });
});

describe('Metadata on decorators', () => {
  it('@Service accepts metadata', () => {
    const meta = new Metadata({ description: 'A test service' });

    @Service({ name: 'MetaSvc', version: 1, metadata: meta })
    class MetaSvc {
      @Rpc()
      async ping(_req: any): Promise<any> { return {}; }
    }

    const info = (MetaSvc as any)[SERVICE_INFO_KEY];
    expect(info.metadata).toBe(meta);
    expect(info.metadata.description).toBe('A test service');
  });

  it('@Rpc accepts metadata', () => {
    @Service({ name: 'RpcMetaSvc', version: 1 })
    class RpcMetaSvc {
      @Rpc({ metadata: new Metadata({ description: 'Does a thing' }) })
      async doThing(_req: any): Promise<any> { return {}; }
    }

    const info = (RpcMetaSvc as any)[SERVICE_INFO_KEY];
    const method = info.methods.get('doThing');
    expect(method).toBeDefined();
    expect(method.metadata).toBeDefined();
    expect(method.metadata.description).toBe('Does a thing');
  });

  it('@ServerStream accepts metadata', () => {
    @Service({ name: 'StreamMetaSvc', version: 1 })
    class StreamMetaSvc {
      @ServerStream({ metadata: new Metadata({ description: 'Watch updates' }) })
      async *watch(_req: any): AsyncGenerator<any> { yield {}; }
    }

    const info = (StreamMetaSvc as any)[SERVICE_INFO_KEY];
    const method = info.methods.get('watch');
    expect(method.metadata.description).toBe('Watch updates');
  });

  it('metadata is undefined when not provided', () => {
    @Service({ name: 'NoMetaSvc', version: 1 })
    class NoMetaSvc {
      @Rpc()
      async bare(_req: any): Promise<any> { return {}; }
    }

    const info = (NoMetaSvc as any)[SERVICE_INFO_KEY];
    expect(info.metadata).toBeUndefined();
    expect(info.methods.get('bare').metadata).toBeUndefined();
  });
});

describe('WireType field metadata', () => {
  it('stores field metadata via WIRE_TYPE_FIELDS_KEY', () => {
    @WireType('test/Invoice', {
      metadata: {
        amount: new Metadata({ description: 'Total in cents' }),
        currency: new Metadata({ description: 'ISO 4217 code' }),
      },
    })
    class Invoice {
      amount = 0;
      currency = 'USD';
    }

    const fields = (Invoice as any)[WIRE_TYPE_FIELDS_KEY];
    expect(fields).toBeDefined();
    expect(fields.amount.description).toBe('Total in cents');
    expect(fields.currency.description).toBe('ISO 4217 code');
  });

  it('no field metadata when not provided', () => {
    @WireType('test/Simple')
    class Simple {
      value = 0;
    }

    expect((Simple as any)[WIRE_TYPE_FIELDS_KEY]).toBeUndefined();
  });
});

// -- RCAN --------------------------------------------------------------------

describe('RCAN validation', () => {
  it('extractCallerRoles parses comma-separated roles', () => {
    const roles = extractCallerRoles({ 'aster.role': 'admin,editor,viewer' });
    expect(roles.has('admin')).toBe(true);
    expect(roles.has('editor')).toBe(true);
    expect(roles.has('viewer')).toBe(true);
    expect(roles.size).toBe(3);
  });

  it('extractCallerRoles returns empty for no role', () => {
    expect(extractCallerRoles({}).size).toBe(0);
  });

  it('evaluateCapability ROLE: passes when role present', () => {
    expect(evaluateCapability(
      { kind: 'role', roles: ['admin'] },
      { 'aster.role': 'admin' },
    )).toBe(true);
  });

  it('evaluateCapability ROLE: fails when role absent', () => {
    expect(evaluateCapability(
      { kind: 'role', roles: ['admin'] },
      { 'aster.role': 'viewer' },
    )).toBe(false);
  });

  it('evaluateCapability ANY_OF: passes with one match', () => {
    expect(evaluateCapability(
      { kind: 'any_of', roles: ['admin', 'editor'] },
      { 'aster.role': 'editor' },
    )).toBe(true);
  });

  it('evaluateCapability ANY_OF: fails with no match', () => {
    expect(evaluateCapability(
      { kind: 'any_of', roles: ['admin', 'editor'] },
      { 'aster.role': 'viewer' },
    )).toBe(false);
  });

  it('evaluateCapability ALL_OF: passes with all roles', () => {
    expect(evaluateCapability(
      { kind: 'all_of', roles: ['admin', 'editor'] },
      { 'aster.role': 'admin,editor,viewer' },
    )).toBe(true);
  });

  it('evaluateCapability ALL_OF: fails with missing role', () => {
    expect(evaluateCapability(
      { kind: 'all_of', roles: ['admin', 'editor'] },
      { 'aster.role': 'admin' },
    )).toBe(false);
  });

  it('validateRcan rejects empty bytes', () => {
    const [valid, reason] = validateRcan(new Uint8Array(0));
    expect(valid).toBe(false);
    expect(reason).toContain('empty');
  });

  it('validateRcan accepts non-empty bytes', () => {
    const [valid] = validateRcan(new Uint8Array([1, 2, 3]));
    expect(valid).toBe(true);
  });

  it('encodeRcan/decodeRcan roundtrip', () => {
    const data = new Uint8Array([42, 99]);
    expect(decodeRcan(encodeRcan(data))).toEqual(data);
  });
});

// -- IID ---------------------------------------------------------------------

describe('IID (cloud identity)', () => {
  it('verifyIID passes when no provider attribute', async () => {
    const [ok] = await verifyIID({});
    expect(ok).toBe(true);
  });

  it('MockIIDBackend passes by default', async () => {
    const backend = new MockIIDBackend();
    const [ok] = await backend.verify({});
    expect(ok).toBe(true);
  });

  it('MockIIDBackend can fail', async () => {
    const backend = new MockIIDBackend({ shouldPass: false, reason: 'test deny' });
    const [ok, reason] = await backend.verify({});
    expect(ok).toBe(false);
    expect(reason).toBe('test deny');
  });

  it('MockIIDBackend checks expected attributes', async () => {
    const backend = new MockIIDBackend({
      expectedAttributes: { [ATTR_IID_ACCOUNT]: '123' },
    });
    const [ok1] = await backend.verify({ [ATTR_IID_ACCOUNT]: '123' });
    expect(ok1).toBe(true);
    const [ok2, reason] = await backend.verify({ [ATTR_IID_ACCOUNT]: '999' });
    expect(ok2).toBe(false);
    expect(reason).toContain('mismatch');
  });

  it('getIIDBackend returns correct backends', () => {
    expect(getIIDBackend('aws')).toBeDefined();
    expect(getIIDBackend('gcp')).toBeDefined();
    expect(getIIDBackend('azure')).toBeDefined();
    expect(getIIDBackend('mock')).toBeDefined();
  });

  it('getIIDBackend throws for unknown provider', () => {
    expect(() => getIIDBackend('unknown')).toThrow('unknown IID provider');
  });

  it('verifyIID with mock backend', async () => {
    const backend = new MockIIDBackend({ shouldPass: true });
    const [ok] = await verifyIID(
      { [ATTR_IID_PROVIDER]: 'mock' },
      backend,
    );
    expect(ok).toBe(true);
  });

  it('verifyIID auto-selects backend from provider attribute', async () => {
    // 'mock' backend always passes
    const [ok] = await verifyIID({ [ATTR_IID_PROVIDER]: 'mock' });
    expect(ok).toBe(true);
  });
});
