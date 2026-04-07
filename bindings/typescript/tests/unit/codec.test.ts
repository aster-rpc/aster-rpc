/**
 * Tests for codec: walkTypeGraph, JsonCodec, ForyCodec.
 */

import { describe, it, expect } from 'vitest';
import {
  JsonCodec,
  walkTypeGraph,
  WireType,
  WIRE_TYPE_KEY,
} from '@aster-rpc/aster';

// -- Test wire types with nesting --

@WireType('test/Inner')
class Inner {
  value = 0;
  constructor(init?: Partial<Inner>) { if (init) Object.assign(this, init); }
}

@WireType('test/Outer')
class Outer {
  name = '';
  inner = new Inner();
  constructor(init?: Partial<Outer>) { if (init) Object.assign(this, init); }
}

@WireType('test/DeepNested')
class DeepNested {
  outer = new Outer();
  tag = '';
  constructor(init?: Partial<DeepNested>) { if (init) Object.assign(this, init); }
}

@WireType('test/Standalone')
class Standalone {
  id = 0;
}

// A class without @WireType
class PlainClass {
  x = 1;
}

@WireType('test/WithArray')
class WithArray {
  items: Inner[] = [];
}

@WireType('test/WithNull')
class WithNull {
  ref: Inner | null = null;
}

describe('walkTypeGraph', () => {
  it('discovers single type with no deps', () => {
    const types = walkTypeGraph([Standalone]);
    expect(types).toEqual([Standalone]);
  });

  it('discovers nested @WireType from default values', () => {
    const types = walkTypeGraph([Outer]);
    // Inner should come before Outer (dependency order)
    expect(types.indexOf(Inner)).toBeLessThan(types.indexOf(Outer));
    expect(types).toContain(Inner);
    expect(types).toContain(Outer);
  });

  it('handles deep nesting (3 levels)', () => {
    const types = walkTypeGraph([DeepNested]);
    expect(types).toContain(Inner);
    expect(types).toContain(Outer);
    expect(types).toContain(DeepNested);
    // Dependency order: Inner < Outer < DeepNested
    expect(types.indexOf(Inner)).toBeLessThan(types.indexOf(Outer));
    expect(types.indexOf(Outer)).toBeLessThan(types.indexOf(DeepNested));
  });

  it('deduplicates when multiple roots share deps', () => {
    const types = walkTypeGraph([Outer, DeepNested, Standalone]);
    // Inner should appear only once
    const innerCount = types.filter(t => t === Inner).length;
    expect(innerCount).toBe(1);
    expect(types.length).toBe(4); // Inner, Outer, DeepNested, Standalone
  });

  it('skips classes without @WireType', () => {
    const types = walkTypeGraph([PlainClass as any]);
    expect(types).toEqual([]);
  });

  it('handles null fields gracefully', () => {
    const types = walkTypeGraph([WithNull]);
    // WithNull has ref=null, so Inner is NOT discovered (limitation)
    expect(types).toContain(WithNull);
    expect(types).not.toContain(Inner);
  });

  it('handles empty array fields', () => {
    const types = walkTypeGraph([WithArray]);
    // WithArray has items=[], so Inner is NOT discovered from empty array
    expect(types).toContain(WithArray);
  });

  it('returns empty for empty input', () => {
    expect(walkTypeGraph([])).toEqual([]);
  });

  it('handles circular references via visited set', () => {
    // Same type appearing multiple times in roots
    const types = walkTypeGraph([Inner, Inner, Inner]);
    expect(types).toEqual([Inner]);
  });
});

describe('JsonCodec', () => {
  it('encode/decode roundtrip', () => {
    const codec = new JsonCodec();
    const obj = { hello: 'world', n: 42 };
    const encoded = codec.encode(obj);
    const decoded = codec.decode(encoded);
    expect(decoded).toEqual(obj);
  });

  it('encodeCompressed returns uncompressed below threshold', () => {
    const codec = new JsonCodec();
    const obj = { small: true };
    const [data, compressed] = codec.encodeCompressed(obj);
    expect(compressed).toBe(false);
    expect(codec.decode(data)).toEqual(obj);
  });

  it('encodeCompressed/decodeCompressed roundtrip for large payloads', () => {
    const codec = new JsonCodec(100); // low threshold for test
    const obj = { data: 'x'.repeat(200) };
    const [data, compressed] = codec.encodeCompressed(obj);
    const decoded = codec.decodeCompressed(data, compressed);
    expect(decoded).toEqual(obj);
  });
});
