/**
 * Canonical `uuid` / `ulid` / `date` / `decimal` / `decimal(p,s)` payload
 * types (Aster-ContractIdentity.md, "Time, UUID, and Decimal").
 *
 * | TypeScript                 | contract type   | wire                         |
 * |----------------------------|-----------------|------------------------------|
 * | {@link uuid} (string)      | `uuid`          | Fory BINARY, exactly 16 bytes |
 * | {@link ulid} (string)      | `ulid`          | Fory BINARY, exactly 16 bytes |
 * | {@link date} (string)      | `date`          | Fory DATE (days since epoch)  |
 * | {@link Decimal}            | `decimal`       | Fory DECIMAL, its own scale   |
 * | {@link decimal}`<P, S>`    | `decimal(P,S)`  | Fory DECIMAL at scale `S`     |
 * | `Date`                     | `timestamp`     | Fory TIMESTAMP (ms in JS)     |
 *
 * Fory JS has no UUID type, so a `uuid` / `ulid` value travels as the
 * 16-byte BINARY Rust and Python send; the codec converts the string form
 * to bytes on encode and back on decode, and refuses any other length.
 * `decimal(p, s)` is rescaled to scale `s` by zero padding on encode and
 * decode; a value that would need rounding or has more than `p` digits is
 * refused with {@link ScalarError}, never rounded.
 *
 * @example
 * ```ts
 * import { WireType, uuid, ulid, date, Decimal, type decimal } from '@aster-rpc/aster';
 *
 * @WireType('shop.v1/Order')
 * class Order {
 *   id: uuid = uuid('0190f2a8-1b2c-7d3e-8f40-123456789abc');
 *   trace: ulid = ulid('01ARZ3NDEKTSV4RRFFQ69G5FAV');
 *   day: date = date('2024-03-01');
 *   price: decimal<18, 4> = Decimal.parse('0');
 * }
 * ```
 */

/**
 * A value outside a canonical scalar type: an identifier that is not 16
 * bytes, a malformed UUID / ULID / date string, or a decimal that does not
 * fit its declared precision and scale.
 */
export class ScalarError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ScalarError';
  }
}

// ── uuid ──────────────────────────────────────────────────────────────────────

/** A UUID in its canonical string form (`8-4-4-4-12` lowercase hex); `uuid` on the wire (16 bytes). */
export type uuid = string & { readonly __asterBrand: 'uuid' };

const UUID_RE = /^[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12}$/i;

function hex(bytes: Uint8Array): string {
  let s = '';
  for (const b of bytes) s += b.toString(16).padStart(2, '0');
  return s;
}

/** Parse and normalise a UUID string (hyphenated or 32 hex digits, any case). */
export function uuid(value: string | Uint8Array): uuid {
  if (value instanceof Uint8Array) return uuidFromBytes(value);
  if (typeof value !== 'string' || !UUID_RE.test(value)) {
    throw new ScalarError(`uuid: not a UUID string: ${JSON.stringify(value)}`);
  }
  const h = value.replace(/-/g, '').toLowerCase();
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}` as uuid;
}

/** The 16 bytes (RFC 9562 order) of a UUID string. */
export function uuidToBytes(value: string): Uint8Array {
  const h = uuid(value).replace(/-/g, '');
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i++) out[i] = parseInt(h.slice(2 * i, 2 * i + 2), 16);
  return out;
}

/** The canonical string of a 16-byte UUID; {@link ScalarError} for any other length. */
export function uuidFromBytes(bytes: Uint8Array): uuid {
  if (!(bytes instanceof Uint8Array) || bytes.length !== 16) {
    throw new ScalarError(`uuid: expected 16 bytes, got ${bytes instanceof Uint8Array ? bytes.length : typeof bytes}`);
  }
  const h = hex(bytes);
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}` as uuid;
}

// ── ulid ──────────────────────────────────────────────────────────────────────

/**
 * A ULID in its string form: 26 characters of Crockford base32, upper case.
 * `ulid` on the wire (16 bytes: 48-bit big-endian ms timestamp, 80 random bits).
 */
export type ulid = string & { readonly __asterBrand: 'ulid' };

const CROCKFORD = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
const CROCKFORD_INDEX = new Map([...CROCKFORD].map((c, i) => [c, i]));

/** Parse and normalise a ULID string (case-insensitive) or 16 bytes. */
export function ulid(value: string | Uint8Array): ulid {
  if (value instanceof Uint8Array) return ulidFromBytes(value);
  return ulidFromBytes(ulidToBytes(value));
}

/** The 16 bytes of a ULID string; {@link ScalarError} when malformed. */
export function ulidToBytes(value: string): Uint8Array {
  const bad = () => new ScalarError(`ulid: not a 26-character Crockford base32 ULID: ${JSON.stringify(value)}`);
  if (typeof value !== 'string' || value.length !== 26) throw bad();
  let n = 0n;
  const upper = value.toUpperCase();
  for (let i = 0; i < 26; i++) {
    const v = CROCKFORD_INDEX.get(upper[i]!);
    // The first character carries the top 3 of 128 bits.
    if (v === undefined || (i === 0 && v > 7)) throw bad();
    n = (n << 5n) | BigInt(v);
  }
  const out = new Uint8Array(16);
  for (let i = 15; i >= 0; i--) {
    out[i] = Number(n & 0xffn);
    n >>= 8n;
  }
  return out;
}

/** The string form of a 16-byte ULID; {@link ScalarError} for any other length. */
export function ulidFromBytes(bytes: Uint8Array): ulid {
  if (!(bytes instanceof Uint8Array) || bytes.length !== 16) {
    throw new ScalarError(`ulid: expected 16 bytes, got ${bytes instanceof Uint8Array ? bytes.length : typeof bytes}`);
  }
  let n = 0n;
  for (const b of bytes) n = (n << 8n) | BigInt(b);
  let s = '';
  for (let i = 0; i < 26; i++) s += CROCKFORD[Number((n >> BigInt(125 - 5 * i)) & 31n)];
  return s as ulid;
}

/** A new ULID for `timestampMs` (default now) with 80 random bits. */
export function generateUlid(timestampMs: number = Date.now()): ulid {
  if (!Number.isSafeInteger(timestampMs) || timestampMs < 0 || timestampMs >= 2 ** 48) {
    throw new ScalarError(`ulid: timestamp ${timestampMs} is not a 48-bit millisecond count`);
  }
  const out = new Uint8Array(16);
  let t = timestampMs;
  for (let i = 5; i >= 0; i--) {
    out[i] = t % 256;
    t = Math.floor(t / 256);
  }
  globalThis.crypto.getRandomValues(out.subarray(6));
  return ulidFromBytes(out);
}

/** The millisecond timestamp in a ULID's first 48 bits. */
export function ulidTimestamp(value: string): number {
  const b = ulidToBytes(value);
  let t = 0;
  for (let i = 0; i < 6; i++) t = t * 256 + b[i]!;
  return t;
}

// ── date ──────────────────────────────────────────────────────────────────────

/** A calendar date with no time zone, `YYYY-MM-DD`; `date` on the wire (Fory DATE). */
export type date = string & { readonly __asterBrand: 'date' };

const DATE_RE = /^(\d{4})-(\d{2})-(\d{2})$/;
const MS_PER_DAY = 86_400_000;

/** Days since 1970-01-01 of a `YYYY-MM-DD` string. */
export function dateToDays(value: string): number {
  const m = typeof value === 'string' ? DATE_RE.exec(value) : null;
  if (m) {
    const [y, mo, d] = [Number(m[1]), Number(m[2]), Number(m[3])];
    const t = new Date(0);
    t.setUTCFullYear(y, mo - 1, d);
    if (t.getUTCFullYear() === y && t.getUTCMonth() === mo - 1 && t.getUTCDate() === d) {
      return Math.round(t.getTime() / MS_PER_DAY);
    }
  }
  throw new ScalarError(`date: not a YYYY-MM-DD calendar date: ${JSON.stringify(value)}`);
}

/** The `YYYY-MM-DD` string of a day count since 1970-01-01. */
export function dateFromDays(days: number): date {
  if (!Number.isSafeInteger(days)) throw new ScalarError(`date: ${days} is not a whole day count`);
  const t = new Date(days * MS_PER_DAY);
  if (Number.isNaN(t.getTime())) throw new ScalarError(`date: day ${days} is outside the JavaScript Date range`);
  const y = t.getUTCFullYear();
  if (y < 0 || y > 9999) throw new ScalarError(`date: year ${y} has no YYYY-MM-DD form`);
  const pad = (n: number, w: number) => String(n).padStart(w, '0');
  return `${pad(y, 4)}-${pad(t.getUTCMonth() + 1, 2)}-${pad(t.getUTCDate(), 2)}` as date;
}

/**
 * A `date` value: a `YYYY-MM-DD` string (validated), or the UTC calendar
 * day of a `Date` that is exactly UTC midnight.
 */
export function date(value: string | Date): date {
  if (value instanceof Date) {
    const ms = value.getTime();
    if (!Number.isFinite(ms) || ms % MS_PER_DAY !== 0) {
      throw new ScalarError(`date: ${value.toISOString?.() ?? String(value)} is not a UTC midnight`);
    }
    return dateFromDays(ms / MS_PER_DAY);
  }
  return dateFromDays(dateToDays(value));
}

// Fory JS 1.7 writes / reads DATE relative to `new Date("1970/01/01 00:00")`,
// which is *local* midnight. Hand it values on that same base so the day
// count on the wire is the true one whatever the process time zone.
const FORY_DATE_EPOCH = new Date('1970/01/01 00:00').getTime();

// ── decimal ───────────────────────────────────────────────────────────────────

/** The largest declared precision of a `decimal(p, s)`. */
export const MAX_DECIMAL_PRECISION = 1000;

const DECIMAL_RE = /^([+-]?)(\d*)(?:\.(\d*))?(?:[eE]([+-]?\d+))?$/;

/**
 * An exact decimal: `unscaledValue × 10^-scale` (Fory DECIMAL's shape).
 * `decimal` on the wire; the field type {@link decimal}`<P, S>` publishes
 * `decimal(P,S)`. Never built from a float: use a string, a bigint or a
 * safe integer.
 */
export class Decimal {
  readonly unscaledValue: bigint;
  readonly scale: number;

  constructor(unscaledValue: bigint | number | string, scale = 0) {
    if (!Number.isInteger(scale)) throw new ScalarError(`decimal: scale ${scale} is not an integer`);
    if (typeof unscaledValue === 'number' && !Number.isSafeInteger(unscaledValue)) {
      throw new TypeError(`decimal: unscaled value ${unscaledValue} is not a safe integer`);
    }
    this.unscaledValue = BigInt(unscaledValue);
    this.scale = scale;
  }

  /** Parse a decimal string: `-12.50`, `1e-3`, `+7`. */
  static parse(text: string): Decimal {
    const m = typeof text === 'string' ? DECIMAL_RE.exec(text.trim()) : null;
    if (!m || (m[2] === '' && (m[3] ?? '') === '')) {
      throw new ScalarError(`decimal: not a decimal: ${JSON.stringify(text)}`);
    }
    const frac = m[3] ?? '';
    const digits = `${m[2]}${frac}` || '0';
    let unscaled = BigInt(digits);
    if (m[1] === '-') unscaled = -unscaled;
    return new Decimal(unscaled, frac.length - Number(m[4] ?? 0));
  }

  /**
   * A Decimal from a Decimal (Aster's or Fory's), a decimal string, a
   * bigint, or a safe integer. A non-integral `number` is refused
   * (`TypeError`): floats are not exact.
   */
  static from(value: unknown): Decimal {
    if (value instanceof Decimal) return value;
    if (typeof value === 'string') return Decimal.parse(value);
    if (typeof value === 'bigint') return new Decimal(value, 0);
    if (typeof value === 'number') {
      if (!Number.isSafeInteger(value)) {
        throw new TypeError(`decimal: ${value} is not a safe integer (pass a string for fractional values)`);
      }
      return new Decimal(BigInt(value), 0);
    }
    if (value !== null && typeof value === 'object'
        && typeof (value as any).unscaledValue === 'bigint' && typeof (value as any).scale === 'number') {
      return new Decimal((value as any).unscaledValue, (value as any).scale);
    }
    throw new TypeError(`decimal: expected Decimal, string, bigint or integer, not ${typeof value}`);
  }

  /** Plain notation at this value's scale: `-12345.6700`, `99999`. */
  toString(): string {
    const neg = this.unscaledValue < 0n;
    let digits = (neg ? -this.unscaledValue : this.unscaledValue).toString();
    let out: string;
    if (this.scale <= 0) {
      out = digits === '0' ? '0' : digits + '0'.repeat(-this.scale);
    } else {
      digits = digits.padStart(this.scale + 1, '0');
      out = `${digits.slice(0, -this.scale)}.${digits.slice(-this.scale)}`;
    }
    return neg ? `-${out}` : out;
  }

  toJSON(): string {
    return this.toString();
  }

  /** Numeric equality (`1.5` equals `1.50`). */
  equals(other: unknown): boolean {
    let o: Decimal;
    try {
      o = Decimal.from(other);
    } catch {
      return false;
    }
    const s = Math.max(this.scale, o.scale);
    return this.unscaledValue * 10n ** BigInt(s - this.scale) === o.unscaledValue * 10n ** BigInt(s - o.scale);
  }
}

/**
 * Field type for `decimal(P,S)`: a {@link Decimal} constrained to `P`
 * significant digits with exactly `S` after the point. The type parameters
 * are what `aster-gen` reads; values are ordinary `Decimal`s.
 */
export type decimal<P extends number, S extends number> = Decimal & {
  readonly __asterDecimal?: readonly [P, S];
};

/** The declared precision and scale of a `decimal(p, s)`. */
export class DecimalSpec {
  constructor(readonly precision: number, readonly scale: number) {
    if (!Number.isInteger(precision) || !Number.isInteger(scale)
        || precision < 1 || precision > MAX_DECIMAL_PRECISION || scale < 0 || scale > precision) {
      throw new Error(
        `decimal(${precision}, ${scale}): needs integers 1 <= p <= ${MAX_DECIMAL_PRECISION} and 0 <= s <= p`,
      );
    }
  }

  /** The canonical contract name, `decimal(p,s)` (no spaces). */
  get typePrimitive(): string {
    return `decimal(${this.precision},${this.scale})`;
  }

  /** Parse `decimal(p,s)`; `undefined` for any other name. */
  static parse(prim: string): DecimalSpec | undefined {
    const m = /^decimal\(([1-9]\d*),(0|[1-9]\d*)\)$/.exec(prim);
    return m ? new DecimalSpec(Number(m[1]), Number(m[2])) : undefined;
  }

  /**
   * `value` at exactly `scale` digits after the point, or
   * {@link ScalarError} when that needs rounding or leaves more than
   * `precision` digits.
   */
  rescale(value: unknown): Decimal {
    const d = Decimal.from(value);
    const p = this.typePrimitive;
    const neg = d.unscaledValue < 0n;
    let unscaled = neg ? -d.unscaledValue : d.unscaledValue;
    if (d.scale > this.scale) {
      const factor = 10n ** BigInt(d.scale - this.scale);
      if (unscaled % factor !== 0n) {
        throw new ScalarError(`${p}: ${d} needs rounding to ${this.scale} decimal places`);
      }
      unscaled /= factor;
    } else {
      unscaled *= 10n ** BigInt(this.scale - d.scale);
    }
    if (unscaled >= 10n ** BigInt(this.precision)) {
      throw new ScalarError(`${p}: ${d} has more than ${this.precision} digits`);
    }
    return new Decimal(neg ? -unscaled : unscaled, this.scale);
  }
}

// ── codec conversion plans ────────────────────────────────────────────────────

/**
 * What one field slot (a scalar, a container element, or a map key) needs
 * from the codec: a canonical scalar to convert, a nested struct (by wire
 * tag) to walk, or nothing.
 * @internal
 */
export type ScalarLeaf =
  | { kind: 'uuid' }
  | { kind: 'ulid' }
  | { kind: 'date' }
  | { kind: 'decimal' }
  | { kind: 'fixed'; spec: DecimalSpec }
  | { kind: 'ref'; tag: string }
  | { kind: 'none' };

/** @internal */
export interface ScalarFieldPlan {
  name: string;
  container: 'none' | 'list' | 'set' | 'map';
  value: ScalarLeaf;
  key?: ScalarLeaf;
}

const NONE: ScalarLeaf = { kind: 'none' };
const plans = new Map<string, readonly ScalarFieldPlan[]>();
let needsMemo = new Map<string, boolean>();

/** The leaf a canonical `type_primitive` needs. @internal */
export function scalarLeafFor(prim: string): ScalarLeaf {
  switch (prim) {
    case 'uuid': return { kind: 'uuid' };
    case 'ulid': return { kind: 'ulid' };
    case 'date': return { kind: 'date' };
    case 'decimal': return { kind: 'decimal' };
  }
  const spec = DecimalSpec.parse(prim);
  return spec ? { kind: 'fixed', spec } : NONE;
}

/**
 * Record how the codec converts the fields of wire type `tag`. Called by
 * `registerGenerated` and the dynamic type factory. @internal
 */
export function registerScalarPlan(tag: string, fields: readonly ScalarFieldPlan[]): void {
  plans.set(tag, fields);
  needsMemo = new Map();
}

function leafNeeds(leaf: ScalarLeaf | undefined, seen: Set<string>): boolean {
  if (!leaf || leaf.kind === 'none') return false;
  if (leaf.kind !== 'ref') return true;
  return reaches(leaf.tag, seen);
}

function reaches(tag: string, seen: Set<string>): boolean {
  if (seen.has(tag)) return false;
  seen.add(tag);
  for (const f of plans.get(tag) ?? []) {
    if (leafNeeds(f.value, seen) || leafNeeds(f.key, seen)) return true;
  }
  return false;
}

/** Whether values of `tag` hold (transitively) a canonical scalar. @internal */
export function scalarsNeeded(tag: string | undefined): boolean {
  if (!tag) return false;
  let v = needsMemo.get(tag);
  if (v === undefined) {
    v = reaches(tag, new Set());
    needsMemo.set(tag, v);
  }
  return v;
}

function encodeLeaf(leaf: ScalarLeaf, v: unknown): unknown {
  if (v === null || v === undefined) return v;
  switch (leaf.kind) {
    case 'uuid':
      if (v instanceof Uint8Array) return uuidToBytes(uuidFromBytes(v));
      if (typeof v !== 'string') throw new ScalarError(`uuid: expected a UUID string, got ${typeof v}`);
      return uuidToBytes(v);
    case 'ulid':
      if (v instanceof Uint8Array) return ulidToBytes(ulidFromBytes(v));
      if (typeof v !== 'string') throw new ScalarError(`ulid: expected a ULID string, got ${typeof v}`);
      return ulidToBytes(v);
    case 'date':
      return FORY_DATE_EPOCH + dateToDays(date(v as string | Date)) * MS_PER_DAY;
    case 'decimal':
      return Decimal.from(v);
    case 'fixed':
      return leaf.spec.rescale(v);
    case 'ref':
      return scalarsToWire(v, leaf.tag);
    case 'none':
      return v;
  }
}

function decodeLeaf(leaf: ScalarLeaf, v: unknown): unknown {
  if (v === null || v === undefined) return v;
  switch (leaf.kind) {
    case 'uuid':
      return uuidFromBytes(v as Uint8Array);
    case 'ulid':
      return ulidFromBytes(v as Uint8Array);
    case 'date': {
      const ms = v instanceof Date ? v.getTime() : Number(v);
      return dateFromDays(Math.round((ms - FORY_DATE_EPOCH) / MS_PER_DAY));
    }
    case 'decimal':
      return Decimal.from(v);
    case 'fixed':
      return leaf.spec.rescale(v);
    case 'ref':
      return scalarsFromWire(v, leaf.tag);
    case 'none':
      return v;
  }
}

function convertField(f: ScalarFieldPlan, v: unknown, leafFn: (l: ScalarLeaf, v: unknown) => unknown): unknown {
  if (v === null || v === undefined) return v;
  const val = f.value.kind === 'none' ? (x: unknown) => x : (x: unknown) => leafFn(f.value, x);
  switch (f.container) {
    case 'none':
      return val(v);
    case 'list':
      return Array.from(v as Iterable<unknown>, val);
    case 'set':
      return new Set(Array.from(v as Iterable<unknown>, val));
    case 'map': {
      const key = !f.key || f.key.kind === 'none' ? (x: unknown) => x : (x: unknown) => leafFn(f.key!, x);
      const entries = v instanceof Map ? v.entries() : Object.entries(v as object);
      const out = new Map();
      for (const [k, x] of entries) out.set(key(k), val(x));
      return out;
    }
  }
}

function fieldNeeds(f: ScalarFieldPlan): boolean {
  const seen = new Set<string>();
  return leafNeeds(f.value, seen) || leafNeeds(f.key, new Set());
}

function convertStruct(
  value: unknown,
  tag: string,
  leafFn: (l: ScalarLeaf, v: unknown) => unknown,
  copy: boolean,
): unknown {
  if (value === null || typeof value !== 'object' || !scalarsNeeded(tag)) return value;
  const src = value as Record<string, unknown>;
  const out = copy ? Object.assign(Object.create(Object.getPrototypeOf(src)), src) : src;
  for (const f of plans.get(tag) ?? []) {
    if (!fieldNeeds(f)) continue;
    try {
      out[f.name] = convertField(f, src[f.name], leafFn);
    } catch (e) {
      if (e instanceof ScalarError && !e.message.startsWith(`${tag}.`)) {
        throw new ScalarError(`${tag}.${f.name}: ${e.message}`);
      }
      throw e;
    }
  }
  return out;
}

/**
 * `value` (a wire type `tag` object) with canonical-scalar values replaced
 * by what Fory writes. Converted structs are shallow copies; `value` is not
 * modified. @internal
 */
export function scalarsToWire(value: unknown, tag: string | undefined): unknown {
  return tag ? convertStruct(value, tag, encodeLeaf, true) : value;
}

/**
 * A freshly decoded `value` with Fory's carriers converted back to the
 * canonical scalars and checked (in place). @internal
 */
export function scalarsFromWire(value: unknown, tag: string | undefined): unknown {
  return tag ? convertStruct(value, tag, decodeLeaf, false) : value;
}
