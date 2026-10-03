/**
 * Golden-vector conformance for the TypeScript RPC codec (plan T2):
 *
 * - conformance/vectors/envelope.json      -- AEv1 StreamHeader / CallHeader / RpcStatus
 * - conformance/vectors/compression.json   -- zstd payload compression (Aster-SPEC §5.6)
 * - conformance/vectors/contract-defaults.json -- @ContractDefault lowering into contract_id
 *
 * The same vectors are checked by Rust core / aster, so passing here means
 * TS produces and accepts the bytes Rust (and Python) do.
 */

import { describe, it, expect, beforeAll } from 'vitest';
import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
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
