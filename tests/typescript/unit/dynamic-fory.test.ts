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
