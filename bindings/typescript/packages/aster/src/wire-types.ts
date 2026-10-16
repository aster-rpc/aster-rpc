/**
 * Canonical wire primitives (Aster-ContractIdentity §11.3.2.3) and their
 * Fory 1.7 type descriptors.
 *
 * The canonical name is what goes into `FieldDef.type_primitive` (and so
 * into `contract_id`); it names the *wire encoding*, not the TypeScript
 * type. Names match the Rust (`aster::rpc::contract` `prim_wire!`) and
 * Python producers byte for byte: `i32` is a zig-zag varint, so it is
 * `"varint32"`, not `"int32"` (which is fixed 4-byte).
 *
 * Fory 1.7's `Type.int32()` / `int64()` / `uint32()` / `uint64()` default to
 * the varint encodings, matching Rust's `i32`/`i64`/`u32`/`u64`.
 */

/** Every `type_primitive` a TypeScript producer can emit or consume. */
export const CANONICAL_PRIMITIVES = [
  'bool',
  'string',
  'binary',
  'int8',
  'int16',
  'int32',
  'varint32',
  'int64',
  'varint64',
  'uint8',
  'uint16',
  'uint32',
  'var_uint32',
  'uint64',
  'var_uint64',
  'float32',
  'float64',
  'timestamp',
  'date',
  'decimal',
  'uuid',
  'ulid',
] as const;

/**
 * A canonical `type_primitive`: one of {@link CANONICAL_PRIMITIVES}, or a
 * constrained decimal `decimal(p,s)` (e.g. `decimal(18,4)`).
 */
export type CanonicalPrimitive = (typeof CANONICAL_PRIMITIVES)[number] | `decimal(${number},${number})`;

/** `decimal(p,s)` in canonical form (base-10, no leading zeros, no spaces). */
const FIXED_DECIMAL_RE = /^decimal\([1-9]\d*,(0|[1-9]\d*)\)$/;

/**
 * The Fory `Type` factory expression for a canonical primitive, as source
 * text (`aster-gen` emits it into the generated file).
 */
export function foryTypeExpr(prim: string): string {
  switch (prim) {
    case 'bool': return 'Type.bool()';
    case 'string': return 'Type.string()';
    case 'binary': return 'Type.binary()';
    case 'int8': return 'Type.int8()';
    case 'int16': return 'Type.int16()';
    case 'varint32': return 'Type.int32()';
    case 'int32': return "Type.int32({ encoding: 'fixed' })";
    case 'varint64': return 'Type.int64()';
    case 'int64': return "Type.int64({ encoding: 'fixed' })";
    case 'uint8': return 'Type.uint8()';
    case 'uint16': return 'Type.uint16()';
    case 'var_uint32': return 'Type.uint32()';
    case 'uint32': return "Type.uint32({ encoding: 'fixed' })";
    case 'var_uint64': return 'Type.uint64()';
    case 'uint64': return "Type.uint64({ encoding: 'fixed' })";
    case 'float32': return 'Type.float32()';
    case 'float64': return 'Type.float64()';
    case 'timestamp': return 'Type.timestamp()';
    case 'date': return 'Type.date()';
    case 'decimal': return 'Type.decimal()';
    // Fory has no UUID type: 16 raw bytes as BINARY, as Rust and Python send.
    case 'uuid':
    case 'ulid': return 'Type.binary()';
    default:
      if (FIXED_DECIMAL_RE.test(prim)) return 'Type.decimal()';
      throw new Error(`unknown canonical wire primitive '${prim}'`);
  }
}

/** Build the Fory `Type` descriptor for a canonical primitive at runtime. */
export function foryTypeFor(prim: string, Type: any): any {
  switch (prim) {
    case 'bool': return Type.bool();
    case 'string': return Type.string();
    case 'binary': return Type.binary();
    case 'int8': return Type.int8();
    case 'int16': return Type.int16();
    case 'varint32': return Type.int32();
    case 'int32': return Type.int32({ encoding: 'fixed' });
    case 'varint64': return Type.int64();
    case 'int64': return Type.int64({ encoding: 'fixed' });
    case 'uint8': return Type.uint8();
    case 'uint16': return Type.uint16();
    case 'var_uint32': return Type.uint32();
    case 'uint32': return Type.uint32({ encoding: 'fixed' });
    case 'var_uint64': return Type.uint64();
    case 'uint64': return Type.uint64({ encoding: 'fixed' });
    case 'float32': return Type.float32();
    case 'float64': return Type.float64();
    case 'timestamp': return Type.timestamp();
    case 'date': return Type.date();
    case 'decimal': return Type.decimal();
    case 'uuid':
    case 'ulid': return Type.binary();
    default:
      if (FIXED_DECIMAL_RE.test(prim)) return Type.decimal();
      throw new Error(`unknown canonical wire primitive '${prim}'`);
  }
}

// -- Contract defaults (§11.3.2.3 "Default lowering into FieldDef") -------------

/** Primitives whose defaults the v1 lowering table defines. */
const DEFAULTABLE = new Set(['string', 'bool', 'varint32', 'varint64', 'float64']);

function hex(bytes: number[]): string {
  return bytes.map(b => b.toString(16).padStart(2, '0')).join('');
}

function varint(v: bigint): number[] {
  const out: number[] = [];
  let x = v;
  while (x >= 0x80n) {
    out.push(Number(x & 0x7fn) | 0x80);
    x >>= 7n;
  }
  out.push(Number(x));
  return out;
}

function zigzag(v: bigint, bits: 32n | 64n): bigint {
  const mask = (1n << bits) - 1n;
  return ((v << 1n) ^ (v >> (bits - 1n))) & mask;
}

function toBigInt(value: unknown, what: string): bigint {
  if (typeof value === 'bigint') return value;
  if (typeof value === 'number' && Number.isInteger(value)) return BigInt(value);
  throw new Error(`contract default for ${what} must be an integer, got ${String(value)}`);
}

/**
 * Lower a declared contract default to `FieldDef.default_value` (hex).
 *
 * @param prim - the field's canonical `type_primitive` (element type for a
 *   container field)
 * @param container - `'none' | 'list' | 'set' | 'map'`
 * @param value - the default; for containers it must be empty
 * @throws on any default the v1 table does not define (reserved wire
 *   types, non-empty containers, non-finite / subnormal floats, out-of-range
 *   integers) -- producers must fail fast (§11.3.2.3 corollary 1).
 */
export function encodeContractDefault(
  prim: string,
  container: 'none' | 'list' | 'set' | 'map',
  value: unknown,
  fieldName = '<field>',
): string {
  const where = `field '${fieldName}'`;
  if (container !== 'none') {
    const size =
      Array.isArray(value) ? value.length
        : value instanceof Set || value instanceof Map ? value.size
          : value !== null && typeof value === 'object' ? Object.keys(value).length
            : -1;
    if (size !== 0) {
      throw new Error(`contract default for ${container} ${where} must be the empty ${container}`);
    }
    return '00';
  }
  if (!DEFAULTABLE.has(prim)) {
    throw new Error(
      `contract default on ${where} of wire type '${prim}' is reserved in v1 ` +
      `(only string, bool, varint32, varint64, float64 and empty containers may carry one)`,
    );
  }
  switch (prim) {
    case 'string': {
      if (typeof value !== 'string') throw new Error(`contract default for ${where} must be a string`);
      const utf8 = [...new TextEncoder().encode(value)];
      return hex([...varint((BigInt(utf8.length) << 2n) | 2n), ...utf8]);
    }
    case 'bool':
      if (typeof value !== 'boolean') throw new Error(`contract default for ${where} must be a boolean`);
      return value ? '01' : '00';
    case 'varint32': {
      const v = toBigInt(value, where);
      if (v < -(2n ** 31n) || v >= 2n ** 31n) throw new Error(`contract default for ${where} out of int32 range`);
      return hex(varint(zigzag(v, 32n)));
    }
    case 'varint64': {
      const v = toBigInt(value, where);
      if (v < -(2n ** 63n) || v >= 2n ** 63n) throw new Error(`contract default for ${where} out of int64 range`);
      return hex(varint(zigzag(v, 64n)));
    }
    case 'float64': {
      if (typeof value !== 'number' || !Number.isFinite(value)) {
        throw new Error(`contract default for ${where} must be a finite number`);
      }
      if (value !== 0 && Math.abs(value) < 2.2250738585072014e-308) {
        throw new Error(`contract default for ${where} must not be subnormal`);
      }
      const buf = new DataView(new ArrayBuffer(8));
      buf.setFloat64(0, value === 0 ? 0 : value, true); // -0.0 -> +0.0
      return hex([...new Uint8Array(buf.buffer)]);
    }
  }
  throw new Error(`unreachable: ${prim}`);
}
