/**
 * Codec abstraction — serialization + optional zstd compression.
 *
 * Spec reference: S5.1 (serialization), S5.2 (compression)
 *
 * Provides:
 * - JsonCodec: JSON over UTF-8 bytes (testing, development)
 * - ForyCodec: @apache-fory/core XLANG (production cross-language)
 * - Zstd compression for payloads > threshold
 */

import { MAX_DECOMPRESSED_SIZE } from './limits.js';
import { WIRE_TYPE_KEY } from './decorators.js';
import { ContractViolationError } from './status.js';
import { getWireShape } from './generated.js';

/** Default compression threshold in bytes (4 KiB). */
export const DEFAULT_COMPRESSION_THRESHOLD = 4096;

/** Re-export WIRE_TYPE_KEY as wireType for API compatibility. */
export { WIRE_TYPE_KEY as wireType };

/** Get the wire type tag from a @WireType-decorated class. */
export function getWireType(cls: unknown): string | undefined {
  return (cls as any)?.[WIRE_TYPE_KEY];
}

/** Generic codec interface for serialization/deserialization. */
export interface Codec {
  encode(obj: unknown, hintType?: unknown): Uint8Array;
  decode(payload: Uint8Array, hintType?: unknown): unknown;
  encodeCompressed(obj: unknown, hintType?: unknown): [data: Uint8Array, compressed: boolean];
  decodeCompressed(payload: Uint8Array, compressed: boolean, hintType?: unknown): unknown;
}

// -- Zstd helpers (use node:zlib which has zstd since Node 21.7) --------------

let _zstdAvailable: boolean | undefined;
let _zstdCompress: ((data: Uint8Array) => Uint8Array) | undefined;
let _zstdDecompress: ((data: Uint8Array, maxSize: number) => Uint8Array) | undefined;

async function initZstd(): Promise<boolean> {
  if (_zstdAvailable !== undefined) return _zstdAvailable;
  try {
    const zlib = await import('node:zlib');
    if (typeof zlib.zstdCompressSync === 'function') {
      _zstdCompress = (data) => new Uint8Array(zlib.zstdCompressSync(Buffer.from(data)));
      _zstdDecompress = (data, maxSize) => {
        const result = zlib.zstdDecompressSync(Buffer.from(data), { maxOutputLength: maxSize });
        if (result.byteLength > maxSize) {
          throw new Error(`decompressed size ${result.byteLength} exceeds limit ${maxSize}`);
        }
        return new Uint8Array(result);
      };
      _zstdAvailable = true;
    } else {
      _zstdAvailable = false;
    }
  } catch {
    _zstdAvailable = false;
  }
  return _zstdAvailable;
}

// Try to init eagerly
initZstd().catch(() => {});

function zstdCompress(data: Uint8Array): Uint8Array | null {
  return _zstdCompress ? _zstdCompress(data) : null;
}

function zstdDecompress(data: Uint8Array): Uint8Array {
  if (!_zstdDecompress) {
    throw new Error('zstd decompression not available (requires Node 21.7+)');
  }
  return _zstdDecompress(data, MAX_DECOMPRESSED_SIZE);
}

// -- JsonCodec ----------------------------------------------------------------

/**
 * Simple JSON codec for cross-language interop and development.
 *
 * Strict mode: when ``decode`` is called with a ``hintType`` argument
 * (a @WireType-decorated class constructor), the codec validates that
 * every key in the decoded object matches a field declared on the
 * class. Unknown keys raise ``ContractViolationError`` -- the producer
 * owns the contract, and consumers must use the field names defined
 * by the producer's manifest. Validation walks nested objects and
 * arrays recursively, so a bad field at any depth fails loudly.
 *
 * If ``hintType`` is omitted (or is ``null``/``undefined``), decoding
 * is permissive and returns the raw parsed value -- the codec doesn't
 * know what shape to enforce. Callers that need strict validation
 * must always pass the expected type.
 */
export class JsonCodec implements Codec {
  private encoder = new TextEncoder();
  private decoder = new TextDecoder();
  private threshold: number;

  constructor(compressionThreshold = DEFAULT_COMPRESSION_THRESHOLD) {
    this.threshold = compressionThreshold;
  }

  encode(obj: unknown): Uint8Array {
    return this.encoder.encode(JSON.stringify(obj));
  }

  decode(payload: Uint8Array, hintType?: unknown): unknown {
    const parsed = JSON.parse(this.decoder.decode(payload));
    if (hintType && typeof hintType === 'function') {
      validateContractShape(parsed, hintType as new (...args: any[]) => any);
    }
    return parsed;
  }

  encodeCompressed(obj: unknown, _hintType?: unknown): [Uint8Array, boolean] {
    const data = this.encode(obj);
    if (data.byteLength > this.threshold && _zstdAvailable) {
      const compressed = zstdCompress(data);
      if (compressed && compressed.byteLength < data.byteLength) {
        return [compressed, true];
      }
    }
    return [data, false];
  }

  decodeCompressed(payload: Uint8Array, compressed: boolean, hintType?: unknown): unknown {
    if (compressed) {
      const decompressed = zstdDecompress(payload);
      return this.decode(decompressed, hintType);
    }
    return this.decode(payload, hintType);
  }
}

/**
 * Cached introspection result for a @WireType class.
 *
 * We cache the field name set + nested-class map per constructor so
 * the validator doesn't `new cls()` on every decode -- that would
 * re-run any side effects in the constructor (e.g. `id =
 * crypto.randomUUID()` initializers, allocator calls). The cache is
 * a WeakMap so it doesn't pin classes that are otherwise garbage.
 *
 * `null` for the cache value means "introspection failed once, don't
 * try again" (e.g. the constructor required positional args). The
 * validator will fall back to permissive decode for that class
 * forever.
 */
interface ClassShape {
  fieldNames: Set<string>;
  /** field name -> nested @WireType class to recurse into, if any. */
  nestedTypes: Map<string, new (...args: any[]) => any>;
  /** field name -> array element @WireType class, if any. */
  elementTypes: Map<string, new (...args: any[]) => any>;
}
const _shapeCache = new WeakMap<new (...args: any[]) => any, ClassShape | null>();

/**
 * Classes we've already warned about for the runtime-introspection
 * fallback. Keyed by constructor so we warn at most once per class
 * per process. This is the signal that aster-gen hasn't been run —
 * if it shows up in server logs, run `bunx aster-gen`.
 */
const _introspectFallbackWarned = new WeakSet<new (...args: any[]) => any>();

function introspectClass(
  cls: new (...args: any[]) => any,
): ClassShape | null {
  const cached = _shapeCache.get(cls);
  if (cached !== undefined) return cached;

  // Prefer the generated shape registry when available — it handles
  // empty arrays, nullable nested types, and non-default-constructible
  // classes (all of which break the runtime `new cls()` path below).
  // TODO(aster-gen): once every user runs `bunx aster-gen` and Fory JS
  // exposes a declarative typeInfo form, the runtime fallback can be
  // deleted entirely. Tracked in ffi_spec/ts-buildtime-audit.md.
  const generated = getWireShape(cls);
  if (generated) {
    const shape: ClassShape = {
      fieldNames: new Set(generated.fieldNameSet),
      nestedTypes: new Map(generated.nestedTypes),
      elementTypes: new Map(generated.elementTypes),
    };
    _shapeCache.set(cls, shape);
    return shape;
  }

  if (!_introspectFallbackWarned.has(cls)) {
    _introspectFallbackWarned.add(cls);
    const name = cls.name || '<anonymous>';
    console.warn(
      `[aster] ${name}: falling back to runtime introspection (new cls() + Object.keys). ` +
      `Run 'npx aster-gen' to get ` +
      `empty-array / nullable-nested / non-default-constructible support and faster decode validation.`,
    );
  }

  let template: any;
  try {
    template = new cls();
  } catch {
    // Class isn't default-constructible -- record a sentinel so we
    // never retry, and fall back to permissive decode forever.
    _shapeCache.set(cls, null);
    return null;
  }

  const fieldNames = new Set(Object.keys(template));
  const nestedTypes = new Map<string, new (...args: any[]) => any>();
  const elementTypes = new Map<string, new (...args: any[]) => any>();

  for (const [key, defaultValue] of Object.entries(template)) {
    if (defaultValue === null || defaultValue === undefined) continue;
    if (Array.isArray(defaultValue)) {
      // For arrays, sample the first element if any. Empty arrays
      // can't be introspected -- documented limitation.
      const sample = defaultValue[0];
      const elementCls = sample?.constructor as
        | (new (...args: any[]) => any)
        | undefined;
      if (
        elementCls &&
        elementCls !== Object &&
        typeof elementCls === 'function'
      ) {
        elementTypes.set(key, elementCls);
      }
      continue;
    }
    if (typeof defaultValue !== 'object') continue; // primitive (incl. enum members)
    const nestedCls = (defaultValue as object).constructor as
      | (new (...args: any[]) => any)
      | undefined;
    if (
      nestedCls &&
      nestedCls !== Object &&
      nestedCls !== Array &&
      nestedCls !== Date &&
      nestedCls !== Map &&
      nestedCls !== Set &&
      typeof nestedCls === 'function'
    ) {
      nestedTypes.set(key, nestedCls);
    }
  }

  const shape: ClassShape = { fieldNames, nestedTypes, elementTypes };
  _shapeCache.set(cls, shape);
  return shape;
}

/**
 * Strict shape validation: walks ``value`` against ``cls`` and throws
 * ``ContractViolationError`` if any object has keys not declared on
 * the corresponding @WireType class. Recurses into nested objects and
 * arrays so a bad field at any depth fails loudly with the dotted
 * path to the violation.
 *
 * Limitations (documented; tests pin them):
 *
 * - Nested types behind a `null` / `undefined` default are not
 *   recursed into. Top-level validation always runs.
 * - Empty array defaults can't be element-introspected. Top-level
 *   validation always runs.
 * - Date / Map / Set / typed-array fields are treated as opaque
 *   primitives -- their values may be objects on the wire but the
 *   validator doesn't try to recurse.
 * - Class generics are erased at runtime; the validator sees the
 *   default value of the generic field, not its declared type.
 */
function validateContractShape(
  value: unknown,
  cls: new (...args: any[]) => any,
  path = '',
): void {
  if (value === null || value === undefined) return;
  if (typeof value !== 'object' || Array.isArray(value)) return;

  const shape = introspectClass(cls);
  if (shape === null) return; // class isn't default-constructible

  const { fieldNames, nestedTypes, elementTypes } = shape;
  const dict = value as Record<string, unknown>;

  const unexpected: string[] = [];
  for (const key of Object.keys(dict)) {
    if (!fieldNames.has(key)) unexpected.push(key);
  }
  if (unexpected.length > 0) {
    const sanitized = sanitizeKeys(unexpected);
    const location = path || cls.name || 'unknown';
    const message =
      `contract violation at ${location}: unexpected JSON field(s) ` +
      `${JSON.stringify(sanitized)} (expected: ${JSON.stringify([...fieldNames].sort())})`;
    throw new ContractViolationError(message, {
      unexpected_fields: sanitized.join(','),
      location,
      expected_class: cls.name || 'unknown',
    });
  }

  // Recurse into nested @WireType objects + arrays using the cached
  // shape map. This avoids re-instantiating the class on every
  // decode (preserving constructor side effects) and is O(1) per
  // field after the first decode.
  for (const [key, child] of Object.entries(dict)) {
    if (child === null || child === undefined) continue;
    const nestedPath = path ? `${path}.${key}` : `${cls.name || 'value'}.${key}`;
    const nestedCls = nestedTypes.get(key);
    if (nestedCls && typeof child === 'object' && !Array.isArray(child)) {
      validateContractShape(child, nestedCls, nestedPath);
      continue;
    }
    const elementCls = elementTypes.get(key);
    if (elementCls && Array.isArray(child)) {
      for (let i = 0; i < child.length; i++) {
        const item = child[i];
        if (item && typeof item === 'object' && !Array.isArray(item)) {
          validateContractShape(item, elementCls, `${nestedPath}[${i}]`);
        }
      }
    }
  }
}

/**
 * Repr-quote unexpected key names for safe logging.
 *
 * Prevents log injection: keys can contain control chars, ANSI
 * escapes, newlines, or backslashes that would corrupt the error
 * message or terminal. We replace control chars with their escape
 * forms, cap each key's length, and cap the number of keys in the
 * list so a malicious client can't blow up log storage with
 * megabyte-long key names.
 */
function sanitizeKeys(keys: string[], maxCount = 5, maxLen = 80): string[] {
  const out: string[] = [];
  for (const k of keys.slice(0, maxCount)) {
    let s = String(k);
    if (s.length > maxLen) s = s.slice(0, maxLen) + '...(truncated)';
    // Escape control chars + non-printable bytes via JSON.stringify
    // (which produces a quoted string with backslash-escapes), then
    // strip the surrounding quotes to keep the inline form readable.
    const quoted = JSON.stringify(s);
    out.push(quoted.slice(1, -1));
  }
  if (keys.length > maxCount) {
    out.push(`...(+${keys.length - maxCount} more)`);
  }
  return out;
}

// -- Type graph walking -------------------------------------------------------

/** Root types that have already triggered the walkTypeGraph fallback warning. */
const _walkGraphFallbackWarned = new WeakSet<new (...args: any[]) => any>();

/**
 * Walk the type graph starting from root types, discovering nested @WireType
 * classes by inspecting default values of instances.
 *
 * Returns all discovered types in dependency order (leaves first), suitable
 * for registration with Fory. This is the TS equivalent of Python's
 * `_walk_type_graph()`.
 *
 * **Deprecation path.** Prefer the generated file from `bunx aster-gen`:
 * it walks the type graph at build time via the TS compiler API and emits
 * a topologically-ordered `WIRE_TYPES` list with full type information,
 * correctly handling empty arrays, optional fields, and non-default-
 * constructible classes. This runtime path warns once per root type when
 * invoked and will be removed after the Fory JS binding exposes a
 * declarative typeInfo schema. Tracked in
 * `ffi_spec/ts-buildtime-audit.md`.
 *
 * Limitations (compared to Python's dataclass introspection):
 * - Only discovers nested types whose default values are instances of @WireType classes
 * - Types in arrays, optionals, or maps that default to empty/null must be registered explicitly
 *
 * @param rootTypes - Classes decorated with @WireType
 * @returns All types in dependency order (leaves first)
 */
export function walkTypeGraph(rootTypes: (new (...args: any[]) => any)[]): (new (...args: any[]) => any)[] {
  for (const cls of rootTypes) {
    if (!_walkGraphFallbackWarned.has(cls)) {
      _walkGraphFallbackWarned.add(cls);
      const name = cls.name || '<anonymous>';
      console.warn(
        `[aster] walkTypeGraph(${name}): runtime type-graph reflection is in use. ` +
        `Run 'npx aster-gen' — the generated ` +
        `WIRE_TYPES list is built from AST types and handles cases this runtime path can't ` +
        `(empty arrays, nullable nested refs, non-default-constructible classes).`,
      );
    }
  }
  const visited = new Set<Function>();
  const ordered: (new (...args: any[]) => any)[] = [];

  function visit(cls: new (...args: any[]) => any): void {
    if (visited.has(cls)) return;
    visited.add(cls);

    // Check this class has a wire type tag
    const tag = (cls as any)[WIRE_TYPE_KEY];
    if (!tag) return;

    // Instantiate to discover fields and their default values
    try {
      const instance = new cls();
      for (const key of Object.keys(instance)) {
        const value = instance[key];
        if (value === null || value === undefined) continue;

        // Check if the value's constructor is a @WireType class
        const ctor = value?.constructor;
        if (ctor && ctor !== Object && ctor !== Array && ctor !== String &&
            ctor !== Number && ctor !== Boolean && (ctor as any)[WIRE_TYPE_KEY]) {
          visit(ctor as new (...args: any[]) => any);
        }

        // Check items in arrays for @WireType instances
        if (Array.isArray(value)) {
          for (const item of value) {
            const itemCtor = item?.constructor;
            if (itemCtor && (itemCtor as any)[WIRE_TYPE_KEY]) {
              visit(itemCtor as new (...args: any[]) => any);
            }
          }
        }
      }
    } catch {
      // If instantiation fails (e.g. required constructor args), skip walking fields
    }

    // Add after dependencies (leaves first)
    ordered.push(cls);
  }

  for (const cls of rootTypes) {
    visit(cls);
  }

  return ordered;
}

// -- ForyConfig ----------------------------------------------------------------

/**
 * Configuration for the Fory serializer.
 */
export class ForyConfig {
  /** Compression threshold in bytes (default: 4096). */
  compressionThreshold: number;
  /** Whether to use cross-language mode (xlang). Default: true. */
  xlang: boolean;

  constructor(opts?: { compressionThreshold?: number; xlang?: boolean }) {
    this.compressionThreshold = opts?.compressionThreshold ?? DEFAULT_COMPRESSION_THRESHOLD;
    this.xlang = opts?.xlang ?? true;
  }

  /** The resolved xlang mode (same as xlang field). */
  get resolvedXlang(): boolean {
    return this.xlang;
  }

  /** Convert to kwargs-style object for passing to Fory constructor. */
  toKwargs(): Record<string, unknown> {
    return {
      xlang: this.xlang,
      // Fory doesn't directly take compressionThreshold; pass via codec
    };
  }
}

/** Resolved ForyConfig with all defaults filled in. */
export interface ResolvedForyConfig {
  compressionThreshold: number;
  xlang: boolean;
  /** Resolved xlang (same as xlang). */
  resolvedXlang: boolean;
}

/**
 * Resolve a ForyConfig with defaults.
 */
export function resolveForyConfig(config?: ForyConfig | { compressionThreshold?: number; xlang?: boolean }): ResolvedForyConfig {
  const xlang = config?.xlang ?? true;
  return {
    compressionThreshold: config?.compressionThreshold ?? DEFAULT_COMPRESSION_THRESHOLD,
    xlang,
    resolvedXlang: xlang,
  };
}

// -- ForyCodec ----------------------------------------------------------------

/**
 * ForyCodec — cross-language serialization via @apache-fory/core.
 *
 * Wraps the Fory JS XLANG serializer for wire-compatible serialization
 * with Python's pyfory.
 *
 * @example
 * ```ts
 * import Fory from '@apache-fory/core';
 * const fory = new Fory({ compatible: true });
 * const codec = new ForyCodec(fory);
 * ```
 */
export class ForyCodec implements Codec {
  readonly fory: any;
  private threshold: number;
  private serializers = new Map<string, { serialize: any; deserialize: any }>();

  constructor(foryInstance: any, compressionThreshold = DEFAULT_COMPRESSION_THRESHOLD) {
    this.fory = foryInstance;
    this.threshold = compressionThreshold;
  }

  /** Register a type for serialization. */
  registerType(typeInfo: any): void {
    const { serialize, deserialize } = this.fory.registerSerializer(typeInfo);
    const name = typeInfo?.options?.typeName ?? typeInfo?.tag ?? String(typeInfo);
    this.serializers.set(name, { serialize, deserialize });
  }

  /**
   * Walk the type graph from root types and register all discovered
   * @WireType classes with Fory. Types are registered in dependency
   * order (leaves first).
   *
   * @param rootTypes - Classes decorated with @WireType
   * @param buildTypeInfo - Function that converts a class to a Fory typeInfo object.
   *   Receives (cls, wireTag) and should return the object to pass to registerType().
   */
  registerTypeGraph(
    rootTypes: (new (...args: any[]) => any)[],
    buildTypeInfo: (cls: new (...args: any[]) => any, wireTag: string) => any,
  ): void {
    const types = walkTypeGraph(rootTypes);
    for (const cls of types) {
      const tag = (cls as any)[WIRE_TYPE_KEY];
      if (tag && !this.serializers.has(tag)) {
        const typeInfo = buildTypeInfo(cls, tag);
        this.registerType(typeInfo);
      }
    }
  }

  encode(obj: unknown, hintType?: unknown): Uint8Array {
    // Convert plain object literals to typed instances when a hint type is provided.
    // This enables cross-language wire compatibility: plain { agent_id: 'x' } becomes
    // `new StatusRequest({ agent_id: 'x' })` so Fory can serialize it with the
    // correct wire tag instead of throwing "Failed to detect the Fory type".
    if (hintType && typeof hintType === 'function' && obj !== null && typeof obj === 'object' && !Array.isArray(obj)) {
      const ctor = hintType as new (...args: any[]) => any;
      obj = new ctor(obj);
    }
    const result = this.fory.serialize(obj);
    return new Uint8Array(result);
  }

  decode(payload: Uint8Array, _hintType?: unknown): unknown {
    return this.fory.deserialize(payload);
  }

  encodeCompressed(obj: unknown, hintType?: unknown): [Uint8Array, boolean] {
    const data = this.encode(obj, hintType);
    if (data.byteLength > this.threshold && _zstdAvailable) {
      const compressed = zstdCompress(data);
      if (compressed && compressed.byteLength < data.byteLength) {
        return [compressed, true];
      }
    }
    return [data, false];
  }

  decodeCompressed(payload: Uint8Array, compressed: boolean, hintType?: unknown): unknown {
    if (compressed) {
      const decompressed = zstdDecompress(payload);
      return this.decode(decompressed, hintType);
    }
    return this.decode(payload, hintType);
  }

  /**
   * Compress bytes using zstd (if available).
   * Returns null if zstd is unavailable or compression didn't help.
   */
  compress(data: Uint8Array): Uint8Array | null {
    if (!_zstdAvailable) return null;
    return zstdCompress(data);
  }

  /**
   * Decompress zstd-compressed bytes.
   */
  decompress(data: Uint8Array): Uint8Array {
    return zstdDecompress(data);
  }

  /**
   * Encode a row schema for a list of field names (for row-oriented data).
   * Returns a JSON-encoded schema descriptor.
   */
  encodeRowSchema(fields: string[]): Uint8Array {
    return new TextEncoder().encode(JSON.stringify({ fields }));
  }

  /**
   * Decode a row-oriented data payload using a previously decoded schema.
   * Reconstructs objects from parallel arrays.
   */
  decodeRowData(schemaBytes: Uint8Array, dataBytes: Uint8Array): unknown[] {
    const schema: { fields: string[] } = JSON.parse(new TextDecoder().decode(schemaBytes));
    const rows: unknown[] = this.decode(dataBytes) as unknown[];
    if (!Array.isArray(rows)) return [];
    return rows.map((row: unknown) => {
      if (!Array.isArray(row)) return row;
      const obj: Record<string, unknown> = {};
      for (let i = 0; i < schema.fields.length; i++) {
        obj[schema.fields[i]!] = row[i];
      }
      return obj;
    });
  }

  /**
   * Return the list of registered type names.
   */
  registeredTypes(): string[] {
    return [...this.serializers.keys()];
  }
}
