/**
 * Tests for codec: walkTypeGraph, ForyCodec (Fory 1.7, xlang + compatible,
 * ref tracking off), and the retired JSON mode.
 */

import { describe, it, expect } from 'vitest';
import * as aster from '@aster-rpc/aster';
import {
  walkTypeGraph,
  WireType,
  WIRE_TYPE_KEY,
  ForyCodec,
  newXlangFory,
  createXlangCodec,
  FORY_OPTIONS,
  FORY_VERSION,
  SerializationMode,
  StatusCode,
  RpcError,
  ContractViolationError,
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

describe('Fory configuration', () => {
  it('matches core/src/xlang.rs::new_fory (compatible, ref tracking off)', () => {
    expect(FORY_OPTIONS).toEqual({ ref: false, compatible: true });
    expect(FORY_VERSION).toBe('1.7.6');
    const { fory } = newXlangFory();
    expect(fory.config.ref).toBe(false);
    expect(fory.config.compatible).toBe(true);
  });

  it('createXlangCodec() is cached; createXlangCodec(fory) wraps that instance', () => {
    expect(createXlangCodec()).toBe(createXlangCodec());
    const { fory } = newXlangFory();
    expect(createXlangCodec(fory).fory).toBe(fory);
  });
});

class Point {
  x = 0;
  label = '';
  tags: string[] = [];
  constructor(init?: Partial<Point>) { if (init) Object.assign(this, init); }
}

// initMeta binds a class to one TypeInfo for good, so build the codec once.
let _pointCodec: ForyCodec | undefined;
function pointCodec(): ForyCodec {
  if (_pointCodec) return _pointCodec;
  const { fory, Type } = newXlangFory();
  const codec = new ForyCodec(fory);
  const t = Type.struct(
    { namespace: 'test.codec', typeName: 'Point' },
    { x: Type.int32(), label: Type.string(), tags: Type.list(Type.string()) },
    { withConstructor: true },
  );
  t.initMeta(Point);
  codec.registerType(t);
  _pointCodec = codec;
  return codec;
}

describe('ForyCodec', () => {
  it('round-trips a registered struct as a class instance', () => {
    const codec = pointCodec();
    const bytes = codec.encode(new Point({ x: -7, label: 'p', tags: ['a', 'b'] }));
    expect(bytes[0]).not.toBe(0x7b); // not JSON
    const back = codec.decode(bytes) as Point;
    expect(back).toBeInstanceOf(Point);
    expect(back).toMatchObject({ x: -7, label: 'p', tags: ['a', 'b'] });
  });

  it('encodes a plain object through the hint type', () => {
    const codec = pointCodec();
    const back = codec.decode(codec.encode({ x: 3, label: 'q', tags: [] }, Point)) as Point;
    expect(back).toBeInstanceOf(Point);
    expect(back.x).toBe(3);
  });

  it('records registered wire tags as namespace/TypeName', () => {
    expect(pointCodec().registeredTypes()).toEqual(['test.codec/Point']);
  });

  it('has no codec-level compression (core framing owns it)', () => {
    const codec = pointCodec() as any;
    expect(codec.encodeCompressed).toBeUndefined();
    expect(codec.decodeCompressed).toBeUndefined();
  });
});

describe('JSON serialization mode is retired', () => {
  it('no JsonCodec / JSON mode in the public API', () => {
    expect((aster as any).JsonCodec).toBeUndefined();
    expect((SerializationMode as any).JSON).toBeUndefined();
    expect((aster as any).SessionServer).toBeUndefined();
  });
});

describe('CONTRACT_VIOLATION status code', () => {
  it('lives in the Aster-native 100+ range', () => {
    expect(StatusCode.CONTRACT_VIOLATION).toBeGreaterThanOrEqual(100);
  });

  it('ContractViolationError IS-A RpcError', () => {
    const err = new ContractViolationError('test');
    expect(err).toBeInstanceOf(RpcError);
    expect(err.code).toBe(StatusCode.CONTRACT_VIOLATION);
  });
});
