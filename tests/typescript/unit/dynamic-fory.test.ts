/**
 * Dynamic Fory registration — mirrors Python's DynamicTypeFactory wire behavior.
 *
 * Proxy-style callers have no generated classes; they receive a manifest from
 * the server and must still Fory-encode requests. The factory must build a
 * Fory Type.struct per manifest wire-tag, link it to a synthesized class via
 * initMeta, and make `codec.encode(instance)` produce Fory bytes (not JSON).
 */

import { describe, it, expect } from 'vitest';
import { DynamicTypeFactory, ForyCodec, newXlangFory } from '@aster-rpc/aster';
import type { ManifestMethod } from '@aster-rpc/aster';

function freshCodec(): { fory: any; Type: any; codec: ForyCodec } {
  const { fory, Type } = newXlangFory();
  const codec = new ForyCodec(fory);
  return { fory, Type, codec };
}

const helloMethod: ManifestMethod = {
  name: 'say_hello',
  pattern: 'unary',
  requestType: 'HelloRequest',
  responseType: 'HelloReply',
  timeout: 5000,
  idempotent: false,
  requestWireTag: 'test.dynamic/HelloRequest',
  responseWireTag: 'test.dynamic/HelloReply',
  fields: [
    { name: 'name', type: 'str', required: true },
    { name: 'count', type: 'int32', required: false, default: 1 },
  ],
  responseFields: [
    { name: 'greeting', type: 'str', required: true },
  ],
};

describe('DynamicTypeFactory + Fory registration', () => {
  it('encodes a manifest-built request as Fory bytes (not JSON)', () => {
    const { fory, Type, codec } = freshCodec();
    const factory = new DynamicTypeFactory();

    factory.registerWithFory([helloMethod], fory, Type, codec);

    const req = factory.buildRequest(helloMethod, { name: 'World', count: 3 });
    const bytes = codec.encode(req);

    expect(bytes.byteLength).toBeGreaterThan(0);
    // JSON would start with '{' (0x7b).
    expect(bytes[0]).not.toBe(0x7b);
    // JSON would contain the field name+value as UTF-8.
    expect(new TextDecoder().decode(bytes).includes('"name":"World"')).toBe(false);
  });

  it('round-trips a manifest-built request through the codec', () => {
    const { fory, Type, codec } = freshCodec();
    const factory = new DynamicTypeFactory();

    factory.registerWithFory([helloMethod], fory, Type, codec);

    const req = factory.buildRequest(helloMethod, { name: 'World', count: 3 });
    const bytes = codec.encode(req);
    const decoded = codec.decode(bytes) as any;

    expect(decoded.name).toBe('World');
    expect(decoded.count).toBe(3);
  });

  it('registers response types and round-trips server-produced shapes', () => {
    const { fory, Type, codec } = freshCodec();
    const factory = new DynamicTypeFactory();

    factory.registerWithFory([helloMethod], fory, Type, codec);

    const ReplyCls = factory.get('test.dynamic/HelloReply');
    expect(ReplyCls).toBeDefined();

    const reply = new (ReplyCls as any)({ greeting: 'Hello, World!' });
    const bytes = codec.encode(reply);
    const decoded = codec.decode(bytes) as any;

    expect(decoded.greeting).toBe('Hello, World!');
  });

  it('is idempotent across multiple registerWithFory calls', () => {
    const { fory, Type, codec } = freshCodec();
    const factory = new DynamicTypeFactory();

    factory.registerWithFory([helloMethod], fory, Type, codec);
    expect(() => factory.registerWithFory([helloMethod], fory, Type, codec)).not.toThrow();

    const req = factory.buildRequest(helloMethod, { name: 'X', count: 7 });
    const decoded = codec.decode(codec.encode(req)) as any;
    expect(decoded.name).toBe('X');
  });
});

describe('DynamicTypeFactory.registerFromTypeDefs (canonical hybrid path)', () => {
  // Use the real NAPI binding to build canonical TypeDef bytes so the
  // round-trip exercises the full scanner → canonical → decode pipeline.
  it('registers a nested REF graph and round-trips through Fory', async () => {
    const { setNativeContract, canonicalXlangBytes, decodeTypeDefBytes, ContractTypeKind, ContainerKind, TypeDefKind } =
      await import('../../../bindings/typescript/packages/aster/dist/index.js');
    const { createRequire } = await import('node:module');
    const { resolve, dirname } = await import('node:path');
    const { existsSync } = await import('node:fs');
    const { fileURLToPath } = await import('node:url');
    const here = dirname(fileURLToPath(import.meta.url));
    const req = createRequire(import.meta.url);
    const nativePath = resolve(here, '../../../bindings/typescript/native/aster-transport.darwin-arm64.node');
    if (!existsSync(nativePath)) {
      // Skip on non-darwin CI; dynamic-fory coverage runs on mac locally.
      return;
    }
    const native = req(nativePath);
    setNativeContract(native);

    // Build two TypeDefs: Inner (leaf) + Outer (holds an Inner ref).
    const innerJson = {
      kind: 'message',
      package: 'test.dynamic',
      name: 'Inner',
      fields: [
        { id: 0, name: 'value', type_kind: 'primitive', type_primitive: 'string',
          type_ref: '', self_ref_name: '', optional: false, ref_tracked: false,
          container: 'none', container_key_kind: 'primitive',
          container_key_primitive: '', container_key_ref: '',
          required: true, default_value: '' },
      ],
      enum_values: [], union_variants: [],
    };
    const innerBytes = native.canonicalBytesFromJson('TypeDef', JSON.stringify(innerJson));
    const innerHash = Array.from(new Uint8Array(native.computeTypeHash(innerBytes)),
      b => b.toString(16).padStart(2, '0')).join('');

    const outerJson = {
      kind: 'message',
      package: 'test.dynamic',
      name: 'Outer',
      fields: [
        { id: 0, name: 'nested', type_kind: 'ref', type_primitive: '',
          type_ref: innerHash, self_ref_name: '', optional: false, ref_tracked: false,
          container: 'none', container_key_kind: 'primitive',
          container_key_primitive: '', container_key_ref: '',
          required: true, default_value: '' },
      ],
      enum_values: [], union_variants: [],
    };
    const outerBytes = native.canonicalBytesFromJson('TypeDef', JSON.stringify(outerJson));
    const outerHash = Array.from(new Uint8Array(native.computeTypeHash(outerBytes)),
      b => b.toString(16).padStart(2, '0')).join('');

    // Decode back and build the two lookup views.
    const inner = decodeTypeDefBytes(innerBytes);
    const outer = decodeTypeDefBytes(outerBytes);
    const byTag = new Map<string, any>([
      ['test.dynamic/Inner', inner],
      ['test.dynamic/Outer', outer],
    ]);
    const byHash = new Map<string, any>([
      [innerHash, inner],
      [outerHash, outer],
    ]);

    const { fory, Type, codec } = freshCodec();
    const factory = new DynamicTypeFactory();
    const resolved = factory.registerFromTypeDefs(
      ['test.dynamic/Outer'],
      { byTag, byHash },
      Type as any,
      codec as any,
    );

    // Both tags resolved via the TypeDef graph — no flat-manifest fallback needed.
    expect(resolved.has('test.dynamic/Inner')).toBe(true);
    expect(resolved.has('test.dynamic/Outer')).toBe(true);

    // Build an Outer instance with a nested Inner and round-trip it.
    const InnerCls = factory.get('test.dynamic/Inner');
    const OuterCls = factory.get('test.dynamic/Outer');
    expect(InnerCls).toBeDefined();
    expect(OuterCls).toBeDefined();
    const innerInst = new (InnerCls as any)({ value: 'hello' });
    const outerInst = new (OuterCls as any)({ nested: innerInst });

    const bytes = codec.encode(outerInst);
    // Should not be JSON.
    expect(bytes[0]).not.toBe(0x7b);

    const decoded = codec.decode(bytes) as any;
    expect(decoded.nested).toBeDefined();
    expect(decoded.nested.value).toBe('hello');
  });
});
