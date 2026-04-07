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
  encodeCompressed(obj: unknown): [data: Uint8Array, compressed: boolean];
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
 * Simple JSON codec for testing and development.
 * Does not support cross-language Fory XLANG wire format.
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

  decode(payload: Uint8Array): unknown {
    return JSON.parse(this.decoder.decode(payload));
  }

  encodeCompressed(obj: unknown): [Uint8Array, boolean] {
    const data = this.encode(obj);
    if (data.byteLength > this.threshold && _zstdAvailable) {
      const compressed = zstdCompress(data);
      if (compressed && compressed.byteLength < data.byteLength) {
        return [compressed, true];
      }
    }
    return [data, false];
  }

  decodeCompressed(payload: Uint8Array, compressed: boolean): unknown {
    if (compressed) {
      const decompressed = zstdDecompress(payload);
      return this.decode(decompressed);
    }
    return this.decode(payload);
  }
}

// -- Type graph walking -------------------------------------------------------

/**
 * Walk the type graph starting from root types, discovering nested @WireType
 * classes by inspecting default values of instances.
 *
 * Returns all discovered types in dependency order (leaves first), suitable
 * for registration with Fory. This is the TS equivalent of Python's
 * `_walk_type_graph()`.
 *
 * Limitations (compared to Python's dataclass introspection):
 * - Only discovers nested types whose default values are instances of @WireType classes
 * - Types in arrays, optionals, or maps that default to empty/null must be registered explicitly
 *
 * @param rootTypes - Classes decorated with @WireType
 * @returns All types in dependency order (leaves first)
 */
export function walkTypeGraph(rootTypes: (new (...args: any[]) => any)[]): (new (...args: any[]) => any)[] {
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

  encode(obj: unknown, _hintType?: unknown): Uint8Array {
    const result = this.fory.serialize(obj);
    return new Uint8Array(result);
  }

  decode(payload: Uint8Array, _hintType?: unknown): unknown {
    return this.fory.deserialize(payload);
  }

  encodeCompressed(obj: unknown): [Uint8Array, boolean] {
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
