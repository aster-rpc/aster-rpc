/**
 * Golden-vector conformance for the TypeScript RPC codec (plan T2):
 *
 * - conformance/vectors/envelope.json      -- AEv1 StreamHeader / CallHeader / RpcStatus
 * - conformance/vectors/compression.json   -- zstd payload compression (Aster-SPEC §5.6)
 * - conformance/vectors/contract-defaults.json -- @ContractDefault lowering into contract_id
 * - conformance/vectors/contract-types.json -- timestamp / date / decimal / decimal(p,s) /
 *   uuid / ulid and recursive types: hashes, contract ids, payload bytes, refusals
 *
 * The same vectors are checked by Rust core / aster, so passing here means
 * TS produces and accepts the bytes Rust (and Python) do.
 */

import { describe, it, expect, beforeAll } from 'vitest';
import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import * as path from 'node:path';
import {
  encodeStreamHeader,
  decodeStreamHeader,
  encodeCallHeader,
  decodeCallHeader,
  encodeRpcStatus,
  decodeRpcStatus,
  encodePayloadFrame,
  decodeFrame,
  payloadOf,
  FramingError,
  COMPRESSED,
  TRAILER,
  HEADER,
  ROW_SCHEMA,
  CALL,
  CANCEL,
  END_STREAM,
  DEFAULT_COMPRESSION_THRESHOLD,
  MAX_DECOMPRESSED_SIZE,
  MAX_FRAME_SIZE,
  encodeContractDefault,
  fromServiceInfo,
  contractIdFromContract,
  setNativeContract,
  registerGenerated,
  getServiceInfo,
  newXlangFory,
  createXlangCodec,
  decodeTypeDefBytes,
  DynamicTypeFactory,
  Decimal,
  ScalarError,
  referenceSnakeCase,
  notPlain,
  plainSuggestion,
  checkFieldNames,
} from '@aster-rpc/aster';
import { loadNative } from '../../../bindings/typescript/packages/aster/src/native.js';

const ROOT = path.resolve(__dirname, '../../..');
const VECTORS = path.join(ROOT, 'conformance/vectors');
const load = (name: string): any => JSON.parse(readFileSync(path.join(VECTORS, name), 'utf8'));

const hex = (b: Uint8Array): string => Buffer.from(b.buffer, b.byteOffset, b.byteLength).toString('hex');
const fromHex = (h: string): Uint8Array => new Uint8Array(Buffer.from(h, 'hex'));
const blake3 = (b: Uint8Array): string => hex(loadNative().computeTypeHash(Buffer.from(b)));

function rule(length: number, r: string): Uint8Array {
  const out = new Uint8Array(length);
  if (r === 'byte[i] = i % 251') for (let i = 0; i < length; i++) out[i] = i % 251;
  else if (r !== 'byte[i] = 0') throw new Error(`unknown payload rule ${r}`);
  return out;
}

// -- core constants ---------------------------------------------------------------

describe('TS constants equal Rust core', () => {
  it('frame flags and limits', () => {
    const n = loadNative();
    expect([COMPRESSED, TRAILER, HEADER, ROW_SCHEMA, CALL, CANCEL, END_STREAM]).toEqual([
      n.FLAG_COMPRESSED, n.FLAG_TRAILER, n.FLAG_HEADER, n.FLAG_ROW_SCHEMA,
      n.FLAG_CALL, n.FLAG_CANCEL, n.FLAG_END_STREAM,
    ]);
    expect(DEFAULT_COMPRESSION_THRESHOLD).toBe(n.COMPRESSION_THRESHOLD);
    expect(MAX_DECOMPRESSED_SIZE).toBe(n.MAX_DECOMPRESSED_SIZE);
    expect(MAX_FRAME_SIZE).toBe(n.MAX_FRAME_SIZE);
    expect(n.ZSTD_LEVEL).toBe(3);
  });
});

// -- envelope.json ------------------------------------------------------------------

const ENCODE: Record<string, (v: any) => Uint8Array> = {
  StreamHeader: encodeStreamHeader,
  CallHeader: encodeCallHeader,
  RpcStatus: encodeRpcStatus,
};
const DECODE: Record<string, (b: Uint8Array) => object> = {
  StreamHeader: decodeStreamHeader,
  CallHeader: decodeCallHeader,
  RpcStatus: decodeRpcStatus,
};

describe('envelope.json (AEv1)', () => {
  const doc = load('envelope.json');

  it('has vectors for all three envelope types', () => {
    const types = new Set(doc.vectors.map((v: any) => v.type));
    expect(types).toEqual(new Set(['StreamHeader', 'CallHeader', 'RpcStatus']));
  });

  for (const v of load('envelope.json').vectors) {
    it(`encodes + decodes: ${v.name}`, () => {
      const bytes = ENCODE[v.type]!(v.input);
      expect(hex(bytes)).toBe(v.hex);
      expect(bytes.byteLength).toBe(v.length);
      expect({ ...DECODE[v.type]!(fromHex(v.hex)) }).toEqual(v.input);
    });
  }

  for (const v of load('envelope.json').invalid) {
    it(`rejects: ${v.name}`, () => {
      expect(() => DECODE[v.type]!(fromHex(v.hex))).toThrow();
    });
  }

  it('refuses to encode out-of-range or mismatched fields', () => {
    expect(() => encodeStreamHeader({ service: 's', method: 'm', deadline: 40_000 })).toThrow(/range/);
    expect(() => encodeStreamHeader({ service: 's', method: 'm', serializationMode: 200 })).toThrow(/range/);
    expect(() => encodeRpcStatus({ code: 0, detailKeys: ['k'], detailValues: [] })).toThrow();
  });

  it('RpcStatus details accessor survives decode', () => {
    const s = decodeRpcStatus(encodeRpcStatus({ code: 5, message: 'nope', detailKeys: ['a'], detailValues: ['b'] }));
    expect(s.code).toBe(5);
    expect(s.details).toEqual({ a: 'b' });
  });
});

// -- compression.json -------------------------------------------------------------

describe('compression.json (zstd, Aster-SPEC §5.6)', () => {
  const doc = load('compression.json');

  it('parameters match core', () => {
    expect(doc.parameters.threshold_bytes).toBe(DEFAULT_COMPRESSION_THRESHOLD);
    expect(doc.parameters.max_decompressed_bytes).toBe(MAX_DECOMPRESSED_SIZE);
    expect(doc.parameters.zstd_level).toBe(3);
  });

  for (const v of load('compression.json').encode_vectors) {
    it(`encode: ${v.name}`, () => {
      const frame = encodePayloadFrame(rule(v.payload_length, v.payload_rule), v.input_flags);
      expect(hex(frame)).toBe(v.expected_frame_hex);
    });
  }

  for (const v of load('compression.json').decode_vectors) {
    it(`decode: ${v.name}`, () => {
      const [payload, flags] = decodeFrame(fromHex(v.frame_hex));
      expect(flags).toBe(v.flags);
      const out = payloadOf(payload, flags);
      expect(out.byteLength).toBe(v.payload_length);
      expect(blake3(out)).toBe(v.payload_blake3);
    });
  }

  for (const v of load('compression.json').reject_vectors) {
    it(`reject: ${v.name}`, () => {
      const [payload, flags] = decodeFrame(fromHex(v.frame_hex));
      expect(() => payloadOf(payload, flags)).toThrow(FramingError);
    });
  }

  it('compresses strictly above the threshold, and only when smaller', () => {
    const compressible = rule(4097, 'byte[i] = i % 251');
    const [p1, f1] = decodeFrame(encodePayloadFrame(compressible, END_STREAM));
    expect(f1).toBe(END_STREAM | COMPRESSED);
    expect(p1.byteLength).toBeLessThan(4097);
    expect(Array.from(payloadOf(p1, f1))).toEqual(Array.from(compressible));

    const random = new Uint8Array(8192);
    for (let i = 0; i < random.length; i++) random[i] = (Math.random() * 256) | 0;
    const [p2, f2] = decodeFrame(encodePayloadFrame(random, 0));
    expect(f2 & COMPRESSED).toBe(0);
    expect(p2.byteLength).toBe(8192);
  });

  it('a caller-supplied COMPRESSED flag never mislabels plain bytes', () => {
    const small = new Uint8Array([1, 2, 3]);
    const [p, f] = decodeFrame(encodePayloadFrame(small, COMPRESSED));
    expect(f & COMPRESSED).toBe(0);
    expect(Array.from(p)).toEqual([1, 2, 3]);
  });
});

// -- contract-defaults.json ---------------------------------------------------------

describe('contract-defaults.json (§11.3.2.3)', () => {
  const doc = load('contract-defaults.json');
  const PKG_ROOT = path.join(ROOT, 'bindings/typescript/packages/aster');
  const FIXTURE = path.join(PKG_ROOT, 'tests/fixtures/defaults');
  const GEN_OUT = path.join(FIXTURE, 'aster-rpc.generated.ts');
  let source = '';

  beforeAll(() => {
    setNativeContract(loadNative() as any);
    const r = spawnSync('node', [
      path.join(PKG_ROOT, 'dist/cli/gen.js'),
      '-p', path.join(FIXTURE, 'tsconfig.json'),
      '-o', GEN_OUT,
    ], { encoding: 'utf8', cwd: PKG_ROOT });
    if (r.status !== 0) throw new Error(`aster-gen failed: ${r.stderr}`);
    source = readFileSync(GEN_OUT, 'utf8');
  });

  it('per-field default_value lowering matches the vector', () => {
    for (const f of doc.vector.type_def.fields_in_declaration_order) {
      if (f.required) continue;
      const container = ['none', 'list', 'set', 'map'][f.container] as 'none' | 'list' | 'set' | 'map';
      const value = {
        state: 'idle', note: '', enabled: true, retries: -3, since_ms: 1_700_000_000_000n,
        ratio: 0.5, label: 'x', tags: [], attrs: new Map(),
      }[f.name as string];
      expect(encodeContractDefault(f.type_primitive, container, value, f.name)).toBe(f.default_value_hex);
    }
  });

  it('aster-gen emits the vector canonical TypeDef bytes and type hash', () => {
    expect(source).toContain(`typeHashHex: "${doc.vector.expected_type_hash}"`);
    const m = source.match(/typeDefBytes: new Uint8Array\(\[([^\]]*)\]\)/);
    expect(m).toBeTruthy();
    const bytes = m![1]!.split(',').map(s => parseInt(s.trim(), 16));
    expect(Buffer.from(bytes).toString('hex')).toBe(doc.vector.expected_canonical_type_def_hex);
  });

  it('the service contract_id matches the vector', async () => {
    const gen = await import(GEN_OUT);
    registerGenerated({ SERVICES: gen.SERVICES, WIRE_TYPES: gen.WIRE_TYPES });
    const svc = gen.SERVICES[0].ctor;
    const contract = fromServiceInfo(getServiceInfo(svc)! as any);
    expect(contract.serializationModes).toEqual(['xlang']);
    expect(contractIdFromContract(contract)).toBe(doc.vector.expected_contract_id);
  });

  it('rejects defaults the v1 table does not define', () => {
    expect(() => encodeContractDefault('int16', 'none', 1)).toThrow(/reserved/);
    expect(() => encodeContractDefault('binary', 'none', new Uint8Array())).toThrow(/reserved/);
    expect(() => encodeContractDefault('string', 'list', ['a'])).toThrow(/empty/);
    expect(() => encodeContractDefault('float64', 'none', Number.NaN)).toThrow(/finite/);
    expect(() => encodeContractDefault('float64', 'none', 5e-324)).toThrow(/subnormal/);
    expect(encodeContractDefault('float64', 'none', -0)).toBe('0000000000000000');
    expect(encodeContractDefault('varint32', 'none', 7)).toBe('0e');
  });
});

// -- contract-types.json ------------------------------------------------------------

describe('contract-types.json (Time, UUID, and Decimal; recursive types)', () => {
  const doc = load('contract-types.json').vector;
  const PKG_ROOT = path.join(ROOT, 'bindings/typescript/packages/aster');
  const FIXTURE = path.join(PKG_ROOT, 'tests/fixtures/contract-types');
  const GEN_OUT = path.join(FIXTURE, 'aster-rpc.generated.ts');
  let gen: any;
  let codec: any;
  const cls = (fqn: string): any => gen.WIRE_TYPES.find((w: any) => w.tag === fqn.replace(/\.([^.]+)$/, '/$1')).ctor;

  beforeAll(async () => {
    setNativeContract(loadNative() as any);
    const r = spawnSync('node', [
      path.join(PKG_ROOT, 'dist/cli/gen.js'),
      '-p', path.join(FIXTURE, 'tsconfig.json'),
      '-o', GEN_OUT,
    ], { encoding: 'utf8', cwd: PKG_ROOT });
    if (r.status !== 0) throw new Error(`aster-gen failed: ${r.stderr}`);
    gen = await import(GEN_OUT);
    const { fory, Type } = newXlangFory();
    codec = createXlangCodec(fory);
    registerGenerated({
      SERVICES: gen.SERVICES, WIRE_TYPES: gen.WIRE_TYPES,
      buildAllTypes: gen.BUILD_ALL_TYPES, fory, Type, codec,
    });
  });

  /** A decoded value in the vector's JSON form (timestamps at JS millisecond precision). */
  const norm = (v: any): any => {
    if (v === null || v === undefined) return null;
    if (v instanceof Date) return v.toISOString();
    if (v instanceof Decimal) return v.toString();
    if (v instanceof Map) return Object.fromEntries([...v].map(([k, x]) => [k, norm(x)]));
    if (v instanceof Set || Array.isArray(v)) return [...v].map(norm);
    if (typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, norm(x)]));
    return v;
  };
  // "2024-03-01T12:34:56.789012Z" -> "2024-03-01T12:34:56.789Z" (Date holds ms; Fory floors).
  const msTimestamp = (t: string | null) => t === null ? null : t.replace(/\.(\d{3})\d*Z$/, '.$1Z');
  const expected = (type: string, value: any): any => type !== 'types.v1.Temporal' ? value : {
    ...value,
    at: msTimestamp(value.at),
    maybe_at: msTimestamp(value.maybe_at),
    history: value.history.map(msTimestamp),
  };

  it('aster-gen emits every vector canonical TypeDef and type hash', () => {
    const src = readFileSync(GEN_OUT, 'utf8');
    for (const [fqn, t] of Object.entries<any>(doc.types)) {
      const shape = gen.WIRE_TYPES.find((w: any) => w.tag === fqn.replace(/\.([^.]+)$/, '/$1'));
      expect(shape, fqn).toBeTruthy();
      expect(shape.typeHashHex, fqn).toBe(t.expected_type_hash);
      expect(hex(shape.typeDefBytes), fqn).toBe(t.expected_canonical_type_def_hex);
    }
    expect(src).toContain('"decimal(18,4)"');
  });

  it('the Types and Ids contract ids match the vector', () => {
    const id = (name: string) => {
      const svc = gen.SERVICES.find((s: any) => s.name === name).ctor;
      return contractIdFromContract(fromServiceInfo(getServiceInfo(svc)! as any));
    };
    expect(id('Types')).toBe(doc.expected_contract_id);
    expect(id('Ids')).toBe(doc.expected_ids_contract_id);
  });

  for (const name of Object.keys(doc.payloads).sort()) {
    it(`decodes Rust payload ${name} and round-trips it`, () => {
      const c = doc.payloads[name];
      const got = codec.decode(fromHex(c.hex), cls(c.type));
      expect(got).toBeInstanceOf(cls(c.type));
      expect(norm(got)).toEqual(expected(c.type, c.value));
      expect(norm(codec.decode(codec.encode(got), cls(c.type)))).toEqual(norm(got));
    });
  }

  // Byte-identical except a string header: Fory JS writes an ASCII string as
  // LATIN1 (0x1c for 7 bytes), Fory Rust as UTF-8 (0x1e). Both decode
  // everywhere; it is not a uuid / ulid / decimal difference (pyfory does
  // the same). Temporal is left out (a JS Date holds milliseconds, the
  // vector microseconds), and so are the recursive payloads: Fory JS writes
  // other type-meta field flags for struct-typed fields than Fory Rust, a
  // codec difference independent of these types; they decode above.
  const asForyJsWrites = (h: string) => h.replace('1e' + Buffer.from('primary').toString('hex'), '1c' + Buffer.from('primary').toString('hex'));
  for (const name of ['identified', 'priced']) {
    it(`writes Rust's bytes for ${name}`, () => {
      const c = doc.payloads[name];
      const got = codec.decode(fromHex(c.hex), cls(c.type));
      expect(hex(codec.encode(got))).toBe(asForyJsWrites(c.hex));
    });
  }

  it('decodes uuid / ulid to their string forms and decimal(p,s) at scale s', () => {
    const id = codec.decode(fromHex(doc.payloads.identified.hex), cls('types.v1.Identified'));
    expect(id.id).toBe('0190f2a8-1b2c-7d3e-8f40-123456789abc');
    expect(id.trace).toBe('01ARZ3NDEKTSV4RRFFQ69G5FAV');
    expect([...id.names.keys()]).toEqual(['0190f2a8-1b2c-7d3e-8f40-123456789abc']);
    const p = codec.decode(fromHex(doc.payloads.priced.hex), cls('types.v1.Priced'));
    expect(p.price).toBeInstanceOf(Decimal);
    expect([p.price.toString(), p.price.scale]).toEqual(['-12345.6700', 4]);
    expect([p.rate.toString(), p.rate.scale]).toEqual(['99999', 0]);
  });

  it('encode rescales to the declared scale, like Rust FixedDecimal', () => {
    const Priced = cls('types.v1.Priced');
    const value = new Priced();
    Object.assign(value, { price: '-12345.67', rate: 99999, maybe_price: Decimal.parse('0.0001'), amount: '1.5' });
    expect(hex(codec.encode(value))).toBe(doc.payloads.priced.hex);
    expect(value.price).toBe('-12345.67'); // the caller's object is not modified
  });

  it('encode refuses a decimal outside its declaration, and floats', () => {
    const Priced = cls('types.v1.Priced');
    const priced = (price: unknown) => Object.assign(new Priced(), { price });
    expect(() => codec.encode(priced('1.23456'))).toThrow(/rounding/);
    expect(() => codec.encode(priced('123456789012345'))).toThrow(/more than 18 digits/);
    expect(() => codec.encode(priced('1.23456'))).toThrow(ScalarError);
    expect(() => codec.encode(priced(1.5))).toThrow(TypeError);
  });

  it('encode refuses a malformed or wrong-length identifier', () => {
    const Identified = cls('types.v1.Identified');
    expect(() => codec.encode(Object.assign(new Identified(), { id: new Uint8Array(15) }))).toThrow(ScalarError);
    expect(() => codec.encode(Object.assign(new Identified(), { trace: '01ARZ3NDEKTSV4RRFFQ69G5FA' }))).toThrow(ScalarError);
    expect(() => codec.encode(Object.assign(new Identified(), { id: 'not-a-uuid' }))).toThrow(ScalarError);
  });

  for (const name of Object.keys(doc.decode_cases).sort()) {
    it(`decode case ${name}`, () => {
      const c = doc.decode_cases[name];
      if (c.refused) {
        expect(() => codec.decode(fromHex(c.hex), cls(c.type))).toThrow(ScalarError);
        return;
      }
      const got = codec.decode(fromHex(c.hex), cls(c.type));
      expect(norm(got)).toEqual(c.value);
      expect(got.price.scale).toBe(4);
    });
  }

  it('the dynamic client builds uuid / ulid / decimal(p,s) fields from Rust TypeDefs', () => {
    const byTag = new Map<string, any>();
    const byHash = new Map<string, any>();
    for (const fqn of ['types.v1.Identified', 'types.v1.Priced']) {
      const t = doc.types[fqn];
      const td = decodeTypeDefBytes(fromHex(t.expected_canonical_type_def_hex));
      byTag.set(`${td.package}/${td.name}`, td);
      byHash.set(t.expected_type_hash, td);
    }
    const { fory, Type } = newXlangFory();
    const dyn = createXlangCodec(fory);
    const factory = new DynamicTypeFactory();
    const roots = ['types.v1/Identified', 'types.v1/Priced'];
    expect(factory.registerFromTypeDefs(roots, { byTag, byHash }, Type, dyn)).toEqual(new Set(roots));
    const DynIdentified = factory.getType('types.v1/Identified')!;
    const DynPriced = factory.getType('types.v1/Priced')!;

    const id = dyn.decode(fromHex(doc.payloads.identified.hex), DynIdentified);
    expect(norm(id)).toEqual(doc.payloads.identified.value);
    expect(hex(dyn.encode(id))).toBe(asForyJsWrites(doc.payloads.identified.hex));
    const p: any = dyn.decode(fromHex(doc.payloads.priced.hex), DynPriced);
    expect(norm(p)).toEqual(doc.payloads.priced.value);
    for (const c of Object.values<any>(doc.decode_cases)) {
      const T = c.type === 'types.v1.Priced' ? DynPriced : DynIdentified;
      if (c.refused) expect(() => dyn.decode(fromHex(c.hex), T)).toThrow(ScalarError);
      else expect(norm(dyn.decode(fromHex(c.hex), T))).toEqual(c.value);
    }
    // What an agent sends: strings for the identifiers and the decimal.
    const req = new DynPriced({ price: '2.5', rate: 3 });
    expect(String((dyn.decode(dyn.encode(req), DynPriced) as any).price)).toBe('2.5000');
    const ids = new DynIdentified({ id: '0190F2A8-1B2C-7D3E-8F40-123456789ABC' });
    expect((dyn.decode(dyn.encode(ids), DynIdentified) as any).id).toBe('0190f2a8-1b2c-7d3e-8f40-123456789abc');
  });
});

// -- contract-types.json field_names (ffi_spec/Aster-FieldNaming.md) ------------------

describe('contract-types.json field_names', () => {
  const fn = load('contract-types.json').vector.field_names;
  const PKG_ROOT = path.join(ROOT, 'bindings/typescript/packages/aster');
  const fixture = (name: string) => path.join(PKG_ROOT, 'tests/fixtures', name);
  const gen = (name: string) => spawnSync('node', [
    path.join(PKG_ROOT, 'dist/cli/gen.js'),
    '-p', path.join(fixture(name), 'tsconfig.json'),
    '-o', path.join(fixture(name), 'aster-rpc.generated.ts'),
  ], { encoding: 'utf8', cwd: PKG_ROOT });

  it("Fory JS's snake_case and plainness match the vector", () => {
    const { TypeMeta } = createRequire(path.join(PKG_ROOT, 'package.json'))(
      '@apache-fory/core/dist/lib/meta/TypeMeta.js');
    for (const [name, row] of Object.entries<any>(fn.snake_case)) {
      expect(TypeMeta.toSnakeCase(name), name).toBe(row.javascript);
      expect(referenceSnakeCase(name), name).toBe(row.rust);
      expect(notPlain(name) === undefined, name).toBe(row.plain);
      if (!row.plain) expect(notPlain(plainSuggestion(name)), name).toBeUndefined();
    }
  });

  it('refuses and accepts the vector cases', () => {
    const entries = (c: any) => c.fields.map((f: any) => ({ memberSnake: f.member_snake, name: f.name }));
    for (const c of fn.refused) expect(() => checkFieldNames(entries(c)), JSON.stringify(c)).toThrow(c.error);
    for (const c of fn.accepted) expect(() => checkFieldNames(entries(c))).not.toThrow();
  });

  it('aster-gen refuses an override that does not match its property', () => {
    const r = gen('field-names-bad');
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain('does not match');
  });

  for (const [def, dir] of [['java_defined', 'field-names-java'], ['rust_defined', 'field-names-rust']] as const) {
    it(`${def}: hashes, contract id and the shared payload`, async () => {
      const r = gen(dir);
      if (r.status !== 0) throw new Error(`aster-gen failed: ${r.stderr}`);
      const g = await import(path.join(fixture(dir), 'aster-rpc.generated.ts'));
      const d = fn[def];
      for (const [fqn, t] of Object.entries<any>(d.types)) {
        const shape = g.WIRE_TYPES.find((w: any) => w.tag === fqn.replace(/\.([^.]+)$/, '/$1'));
        expect(shape.typeHashHex, fqn).toBe(t.expected_type_hash);
        expect(hex(shape.typeDefBytes), fqn).toBe(t.expected_canonical_type_def_hex);
      }
      const { fory, Type } = newXlangFory();
      const codec = createXlangCodec(fory);
      registerGenerated({ SERVICES: g.SERVICES, WIRE_TYPES: g.WIRE_TYPES, buildAllTypes: g.BUILD_ALL_TYPES, fory, Type, codec });
      const svc = g.SERVICES.find((s: any) => s.name === 'Orders').ctor;
      expect(contractIdFromContract(fromServiceInfo(getServiceInfo(svc)! as any))).toBe(d.expected_contract_id);
      const Order = g.WIRE_TYPES.find((w: any) => w.tag === 'names.v1/Order').ctor;
      const got = codec.decode(fromHex(d.payload.hex), Order);
      // The properties are camelCase in both fixtures, whatever the
      // contract's names (orderId or order_id): Fory matches by snake_case.
      const plain = JSON.parse(JSON.stringify(got));
      expect(plain).toEqual({
        orderId: 'o-1',
        lineItems: [{ skuCode: 'sku-a', unitCount: 2 }, { skuCode: 'sku-b', unitCount: 1 }],
        totalCents: 1250,
        note: null,
      });
      expect(JSON.parse(JSON.stringify(codec.decode(codec.encode(got), Order)))).toEqual(plain);
    });
  }
});
