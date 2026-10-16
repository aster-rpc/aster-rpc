/**
 * Canonical uuid / ulid / date / decimal / decimal(p,s) values
 * (Aster-ContractIdentity.md, "Time, UUID, and Decimal"). The wire bytes
 * against Rust are pinned by conformance-vectors.test.ts (contract-types.json).
 */

import { describe, it, expect } from 'vitest';
import { spawnSync } from 'node:child_process';
import * as path from 'node:path';
import {
  uuid, uuidToBytes, uuidFromBytes,
  ulid, ulidToBytes, ulidFromBytes, generateUlid, ulidTimestamp,
  date, dateToDays, dateFromDays,
  Decimal, DecimalSpec, ScalarError,
  foryTypeFor, CANONICAL_PRIMITIVES,
  newXlangFory, createXlangCodec, registerGenerated,
} from '@aster-rpc/aster';
import { foryTypeExpr } from '../../../bindings/typescript/packages/aster/src/wire-types.js';

describe('uuid', () => {
  it('normalises to lowercase 8-4-4-4-12 and converts to RFC 9562 bytes', () => {
    const u = uuid('0190F2A81B2C7D3E8F40123456789ABC');
    expect(u).toBe('0190f2a8-1b2c-7d3e-8f40-123456789abc');
    const b = uuidToBytes(u);
    expect(Buffer.from(b).toString('hex')).toBe('0190f2a81b2c7d3e8f40123456789abc');
    expect(uuidFromBytes(b)).toBe(u);
  });

  it('refuses malformed strings and other lengths', () => {
    expect(() => uuid('0190f2a8-1b2c-7d3e-8f40-123456789ab')).toThrow(ScalarError);
    expect(() => uuid('g190f2a8-1b2c-7d3e-8f40-123456789abc')).toThrow(ScalarError);
    expect(() => uuidFromBytes(new Uint8Array(15))).toThrow(/16 bytes/);
    expect(() => uuidFromBytes(new Uint8Array(17))).toThrow(ScalarError);
  });
});

describe('ulid', () => {
  it('has the Crockford base32 string form (case-insensitive in, upper out)', () => {
    const u = ulid('01arz3ndektsv4rrffq69g5fav');
    expect(u).toBe('01ARZ3NDEKTSV4RRFFQ69G5FAV');
    expect(ulidTimestamp(u)).toBe(1469922850259);
    expect(ulidFromBytes(new Uint8Array(16).fill(0xff))).toBe('7ZZZZZZZZZZZZZZZZZZZZZZZZZ');
    expect(ulidFromBytes(ulidToBytes(u))).toBe(u);
  });

  it('refuses a first character above 7, letters outside the alphabet, and other lengths', () => {
    for (const bad of ['8ZZZZZZZZZZZZZZZZZZZZZZZZZ', '01ARZ3NDEKTSV4RRFFQ69G5FAU', '01ARZ3NDEKTSV4RRFFQ69G5FA']) {
      expect(() => ulid(bad), bad).toThrow(ScalarError);
    }
    expect(() => ulidFromBytes(new Uint8Array(15))).toThrow(ScalarError);
  });

  it('generates a ULID carrying the timestamp', () => {
    const u = generateUlid(1_700_000_000_000);
    expect(u).toHaveLength(26);
    expect(ulidTimestamp(u)).toBe(1_700_000_000_000);
    expect(generateUlid(1_700_000_000_000)).not.toBe(u);
  });
});

describe('date', () => {
  it('converts YYYY-MM-DD to days since 1970-01-01 and back', () => {
    expect(dateToDays('1970-01-01')).toBe(0);
    expect(dateToDays('2024-03-01')).toBe(19783);
    expect(dateToDays('1969-12-31')).toBe(-1);
    expect(dateFromDays(19783)).toBe('2024-03-01');
    expect(date(new Date(Date.UTC(2024, 2, 1)))).toBe('2024-03-01');
  });

  it('refuses impossible dates and Dates that are not UTC midnight', () => {
    expect(() => date('2024-02-30')).toThrow(ScalarError);
    expect(() => date('2024-3-1')).toThrow(ScalarError);
    expect(() => date(new Date(Date.UTC(2024, 2, 1, 12)))).toThrow(ScalarError);
  });

  it('puts the true day count on the wire in any process time zone', () => {
    // Fory JS 1.7 counts DATE days from local midnight; the codec corrects for it.
    const dist = path.resolve(__dirname, '../../../bindings/typescript/packages/aster/dist/index.js');
    const script = `
      const m = await import(${JSON.stringify(dist)});
      const { fory, Type } = m.newXlangFory();
      const codec = m.createXlangCodec(fory);
      class D { constructor() { this.day = '1970-01-01'; } }
      m.registerGenerated({ SERVICES: [], WIRE_TYPES: [{ ctor: D, tag: 't.v1/D',
        fields: [{ name: 'day', kind: 'primitive', wire: 'date', nullable: false }],
        fieldNameSet: new Set(['day']), nestedTypes: new Map(), elementTypes: new Map() }] });
      const ts = Type.struct({ namespace: 't.v1', typeName: 'D' }, { day: Type.date() }, { withConstructor: true });
      ts.initMeta(D); codec.registerType(ts);
      const d = new D(); d.day = '2024-03-01';
      const bytes = codec.encode(d);
      console.log(Buffer.from(bytes).toString('hex') + ' ' + codec.decode(bytes).day);
    `;
    const outs = ['UTC', 'America/Los_Angeles', 'Asia/Kolkata', 'Pacific/Kiritimati'].map(tz => {
      const r = spawnSync('node', ['--input-type=module', '-e', script], {
        encoding: 'utf8', env: { ...process.env, TZ: tz },
      });
      expect(r.status, r.stderr).toBe(0);
      return r.stdout.trim();
    });
    expect(new Set(outs).size).toBe(1);
    expect(outs[0]!.endsWith(' 2024-03-01')).toBe(true);
  });
});

describe('Decimal and decimal(p,s)', () => {
  it('parses and prints plain notation at the value scale', () => {
    expect(Decimal.parse('-12345.6700').toString()).toBe('-12345.6700');
    expect(Decimal.parse('1e3').toString()).toBe('1000');
    expect(Decimal.parse('.5').toString()).toBe('0.5');
    expect(new Decimal(1n, 4).toString()).toBe('0.0001');
    expect(Decimal.from(12n).toString()).toBe('12');
    expect(Decimal.parse('1.5').equals('1.50')).toBe(true);
    expect(() => Decimal.parse('1.2.3')).toThrow(ScalarError);
    expect(() => Decimal.from(1.5)).toThrow(TypeError);
  });

  it('rescales by zero padding and refuses rounding or too many digits', () => {
    const money = DecimalSpec.parse('decimal(18,4)')!;
    expect(money.typePrimitive).toBe('decimal(18,4)');
    expect(money.rescale('1.5').toString()).toBe('1.5000');
    expect(money.rescale('2.50000').toString()).toBe('2.5000');
    expect(() => money.rescale('1.23456')).toThrow(/rounding/);
    expect(() => money.rescale('123456789012345')).toThrow(/more than 18 digits/);
    expect(new DecimalSpec(5, 0).rescale('99999').toString()).toBe('99999');
    expect(() => new DecimalSpec(5, 0).rescale('100000')).toThrow(ScalarError);
    expect(DecimalSpec.parse('decimal(018,4)')).toBeUndefined();
    expect(DecimalSpec.parse('decimal(18, 4)')).toBeUndefined();
    expect(() => new DecimalSpec(4, 5)).toThrow();
  });
});

describe('wire types', () => {
  it('uuid and ulid are 16-byte BINARY, date DATE, decimals DECIMAL', () => {
    const { Type } = newXlangFory();
    for (const p of ['uuid', 'ulid']) {
      expect(foryTypeFor(p, Type).typeId).toBe(Type.binary().typeId);
      expect(foryTypeExpr(p)).toBe('Type.binary()');
    }
    expect(foryTypeFor('date', Type).typeId).toBe(Type.date().typeId);
    expect(foryTypeFor('decimal', Type).typeId).toBe(Type.decimal().typeId);
    expect(foryTypeFor('decimal(18,4)', Type).typeId).toBe(Type.decimal().typeId);
    expect(() => foryTypeFor('decimal(18, 4)', Type)).toThrow();
    expect(CANONICAL_PRIMITIVES).toEqual(expect.arrayContaining(['uuid', 'ulid', 'date', 'decimal', 'timestamp']));
  });

  it('a uuid list, a uuid-keyed map and a nullable decimal(p,s) round-trip; the caller object is kept', () => {
    const { fory, Type } = newXlangFory();
    const codec = createXlangCodec(fory);
    class Holder {
      ids: string[] = [];
      byId = new Map<string, string>();
      price: unknown = null;
    }
    registerGenerated({
      SERVICES: [],
      WIRE_TYPES: [{
        ctor: Holder, tag: 't.v1/Holder',
        fields: [
          { name: 'ids', kind: 'list', element: { name: 'ids[]', kind: 'primitive', wire: 'uuid', nullable: false }, nullable: false },
          { name: 'byId', kind: 'map', key: { name: 'k', kind: 'primitive', wire: 'uuid', nullable: false }, value: { name: 'v', kind: 'primitive', wire: 'string', nullable: false }, nullable: false },
          { name: 'price', kind: 'primitive', wire: 'decimal(10,2)', nullable: true },
        ],
        fieldNameSet: new Set(['ids', 'byId', 'price']), nestedTypes: new Map(), elementTypes: new Map(),
        foryTypeInfo: undefined,
      }],
    });
    const ts = Type.struct({ namespace: 't.v1', typeName: 'Holder' }, {
      ids: Type.list(Type.binary()),
      byId: Type.map(Type.binary(), Type.string()),
      price: Type.decimal().setNullable(true),
    }, { withConstructor: true });
    ts.initMeta(Holder);
    codec.registerType(ts);

    const h = new Holder();
    h.ids = ['0190F2A8-1B2C-7D3E-8F40-123456789ABC'];
    h.byId.set('00000000-0000-0000-0000-000000000001', 'one');
    h.price = '3.5';
    const back = codec.decode(codec.encode(h)) as Holder;
    expect(back).toBeInstanceOf(Holder);
    expect(back.ids).toEqual(['0190f2a8-1b2c-7d3e-8f40-123456789abc']);
    expect([...back.byId]).toEqual([['00000000-0000-0000-0000-000000000001', 'one']]);
    expect(String(back.price)).toBe('3.50');
    expect(h.ids[0]).toBe('0190F2A8-1B2C-7D3E-8F40-123456789ABC');
    h.price = null;
    expect((codec.decode(codec.encode(h)) as Holder).price).toBeNull();
    h.ids = ['nope'];
    expect(() => codec.encode(h)).toThrow(/t\.v1\/Holder\.ids: uuid/);
  });
});
