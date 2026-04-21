/**
 * DynamicTypeFactory — synthesize wire-compatible types from contract manifests.
 *
 * Enables calling remote services without local type definitions.
 * Given a ContractManifest, this factory creates JavaScript classes
 * with the correct field names and Fory wire tags.
 *
 * Used by the shell, MCP server, and dynamic clients.
 */

import { WIRE_TYPE_KEY } from './decorators.js';
import type { ManifestMethod, ManifestField } from './contract/manifest.js';

/** A dynamically synthesized type class. */
export interface DynamicType {
  new (init?: Record<string, unknown>): Record<string, unknown>;
  wireTag: string;
}

/**
 * Create a dynamic type class from a wire tag and field descriptors.
 *
 * The returned class:
 * - Has the correct wire tag (set via WIRE_TYPE_KEY symbol)
 * - Accepts a partial init object in the constructor
 * - Has default values for all fields based on their type
 */
export function createDynamicType(wireTag: string, fields: ManifestField[]): DynamicType {
  // Build default values
  const defaults: Record<string, unknown> = {};
  for (const field of fields) {
    defaults[field.name] = field.default ?? defaultForField(field);
  }

  // Create the class dynamically
  const DynClass = class {
    static wireTag = wireTag;

    constructor(init?: Record<string, unknown>) {
      // Apply defaults
      for (const [key, value] of Object.entries(defaults)) {
        (this as any)[key] = value;
      }
      // Apply init overrides
      if (init) {
        for (const [key, value] of Object.entries(init)) {
          (this as any)[key] = value;
        }
      }
    }
  };

  // Set wire type tag
  (DynClass as any)[WIRE_TYPE_KEY] = wireTag;

  // Set the class name for debugging
  Object.defineProperty(DynClass, 'name', { value: wireTag.split('/').pop() ?? wireTag });

  return DynClass as unknown as DynamicType;
}

/**
 * Split a wire tag into (namespace, typeName) for Fory Type.struct options.
 * Accepts both "/" and "." separators; the last segment is the type name.
 */
function splitWireTag(tag: string): { namespace: string; typeName: string } {
  const slash = tag.lastIndexOf('/');
  if (slash >= 0) return { namespace: tag.slice(0, slash), typeName: tag.slice(slash + 1) };
  const dot = tag.lastIndexOf('.');
  if (dot >= 0) return { namespace: tag.slice(0, dot), typeName: tag.slice(dot + 1) };
  return { namespace: '', typeName: tag };
}

/** Map a v1-schema manifest field dict to a Fory Type node.
 *
 * v1 fields have a `kind` ("string" | "int" | "float" | "bool" | "bytes" |
 * "list" | "map" | "ref" | "enum") with matching *_kind siblings for
 * containers. Legacy fields only have `type: "str"`-style strings;
 * fall back to `foryFieldTypeFromString` for those.
 */
function foryFieldTypeFromField(f: ManifestField, Type: any): any {
  const kind = (f as any).kind;
  if (typeof kind !== 'string') {
    return foryFieldTypeFromString((f as any).type ?? 'string', Type);
  }
  switch (kind) {
    case 'string':
      return Type.string();
    case 'int':
      return Type.int32();
    case 'float':
      return Type.float64();
    case 'bool':
      return Type.bool();
    case 'bytes':
      return Type.binary();
    case 'enum':
      return Type.string();
    case 'list': {
      const itemKind = (f as any).item_kind ?? 'string';
      const itemType = foryFieldTypeFromString(itemKind, Type);
      return Type.array(itemType);
    }
    case 'map': {
      const keyKind = (f as any).key_kind ?? 'string';
      const valKind = (f as any).value_kind ?? 'string';
      return Type.map(
        foryFieldTypeFromString(keyKind, Type),
        foryFieldTypeFromString(valKind, Type),
      );
    }
    case 'ref':
      // Best-effort: unknown struct; fall through to the string default so
      // decoding doesn't crash on unregistered inner types. Server-side
      // xlang will still reject mismatches via the Fory hash check, so the
      // call surfaces a clear error rather than a silent mis-decode.
      return Type.string();
    default:
      return Type.string();
  }
}

/** Map a manifest field type string to a Fory Type node (legacy + inner-kind). */
function foryFieldTypeFromString(typeStr: string, Type: any): any {
  const lower = (typeStr || 'string').toLowerCase();
  if (lower.startsWith('list[') || lower.startsWith('array[')) {
    const inner = typeStr.slice(typeStr.indexOf('[') + 1, typeStr.lastIndexOf(']'));
    return Type.array(foryFieldTypeFromString(inner, Type));
  }
  if (lower.startsWith('optional[') || lower.startsWith('nullable[')) {
    const inner = typeStr.slice(typeStr.indexOf('[') + 1, typeStr.lastIndexOf(']'));
    return foryFieldTypeFromString(inner, Type);
  }
  if (lower.startsWith('dict[') || lower.startsWith('map[')) {
    // Keys default to string in both Python and TS manifest flows.
    const body = typeStr.slice(typeStr.indexOf('[') + 1, typeStr.lastIndexOf(']'));
    const comma = body.indexOf(',');
    const valType = comma >= 0 ? body.slice(comma + 1).trim() : 'str';
    return Type.map(Type.string(), foryFieldTypeFromString(valType, Type));
  }
  switch (lower) {
    case 'str':
    case 'string':
      return Type.string();
    case 'int':
    case 'int32':
      return Type.int32();
    case 'int64':
      return Type.int64();
    case 'int16':
      return Type.int16();
    case 'int8':
      return Type.int8();
    case 'float':
    case 'float64':
    case 'double':
      return Type.float64();
    case 'float32':
      return Type.float32();
    case 'bool':
    case 'boolean':
      return Type.bool();
    case 'bytes':
    case 'binary':
      return Type.binary();
    default:
      // Unknown scalar: default to string so decoding still surfaces the value.
      return Type.string();
  }
}

/**
 * Factory that synthesizes types for all methods in a manifest.
 *
 * Returns a map of wire tag -> DynamicType for both request and response types.
 */
export class DynamicTypeFactory {
  private types = new Map<string, DynamicType>();
  /** Wire tags that have been registered with a Fory instance via registerWithFory. */
  private foryRegistered = new Set<string>();

  /**
   * Synthesize types for a method's request and response.
   * Returns [RequestType, ResponseType] or undefined if wire tags are missing.
   */
  synthesizeForMethod(method: ManifestMethod): [DynamicType, DynamicType] | undefined {
    if (!method.requestWireTag || !method.responseWireTag) return undefined;

    const reqType = this.getOrCreate(method.requestWireTag, method.fields);
    const respType = this.getOrCreate(method.responseWireTag, method.responseFields ?? []);

    return [reqType, respType];
  }

  /**
   * Synthesize classes for each method's request/response types, build a
   * Fory `Type.struct` per wire tag, and register with the provided codec
   * so that `codec.encode(instance)` produces Fory bytes rather than JSON.
   *
   * Mirrors Python's `DynamicTypeFactory.register_from_manifest` followed
   * by `ForyCodec(types=factory.get_all_types())`, but for TS's register-
   * by-name Fory API.
   *
   * Idempotent per (factory, wire-tag) — calling twice with the same tag
   * is a no-op after the first call.
   */
  registerWithFory(
    methods: ManifestMethod[],
    _fory: { registerSerializer: (t: unknown) => unknown },
    Type: { struct: (opts: unknown, fields: unknown, meta?: unknown) => any },
    codec: { registerType(typeInfo: unknown): void },
  ): void {
    for (const m of methods) {
      if (m.requestWireTag) {
        this.registerOne(m.requestWireTag, m.fields ?? [], Type, codec);
      }
      if (m.responseWireTag) {
        this.registerOne(m.responseWireTag, m.responseFields ?? [], Type, codec);
      }
    }
  }

  private registerOne(
    wireTag: string,
    fields: ManifestField[],
    Type: { struct: (opts: unknown, fields: unknown, meta?: unknown) => any },
    codec: { registerType(typeInfo: unknown): void },
  ): void {
    if (this.foryRegistered.has(wireTag)) return;
    const cls = this.getOrCreate(wireTag, fields);

    const foryFields: Record<string, unknown> = {};
    for (const f of fields) {
      foryFields[f.name] = foryFieldTypeFromField(f, Type);
    }
    const { namespace, typeName } = splitWireTag(wireTag);
    const typeStruct = Type.struct({ namespace, typeName }, foryFields, { withConstructor: true });

    // Link the Fory typeInfo to the synthesized class so Fory can look up
    // the type from an instance's constructor at serialize time.
    (typeStruct as { initMeta: (ctor: unknown) => void }).initMeta(cls);
    codec.registerType(typeStruct);
    this.foryRegistered.add(wireTag);
  }

  /** Get a previously synthesized type by wire tag. */
  get(wireTag: string): DynamicType | undefined {
    return this.types.get(wireTag);
  }

  /** Get a type by wire tag (alias for get()). */
  getType(wireTag: string): DynamicType | undefined {
    return this.types.get(wireTag);
  }

  /** Return all synthesized types. */
  getAllTypes(): DynamicType[] {
    return [...this.types.values()];
  }

  /** Number of registered types. */
  get typeCount(): number {
    return this.types.size;
  }

  /**
   * Register types from a manifest's method descriptors.
   * Synthesizes request and response types for all methods.
   */
  registerFromManifest(manifest: { methods: ManifestMethod[] }): void {
    for (const method of manifest.methods) {
      this.synthesizeForMethod(method);
    }
  }

  /**
   * Build a request object for a method using default values.
   * Returns an instance of the synthesized request type.
   */
  buildRequest(method: ManifestMethod, overrides?: Record<string, unknown>): unknown {
    if (!method.requestWireTag) return overrides ?? {};
    let type = this.types.get(method.requestWireTag);
    if (!type && method.fields) {
      type = createDynamicType(method.requestWireTag, method.fields);
      this.types.set(method.requestWireTag, type);
    }
    if (!type) return overrides ?? {};
    const instance = new (type as any)(overrides);
    return instance;
  }

  /** All synthesized types. */
  allTypes(): IterableIterator<[string, DynamicType]> {
    return this.types.entries();
  }

  private getOrCreate(wireTag: string, fields: ManifestField[]): DynamicType {
    let type = this.types.get(wireTag);
    if (!type) {
      type = createDynamicType(wireTag, fields);
      this.types.set(wireTag, type);
    }
    return type;
  }
}

/** Get a default value from a v1-schema field dict, falling back to the legacy
 *  type-string form. */
function defaultForField(field: ManifestField): unknown {
  const kind = (field as any).kind;
  if (typeof kind === 'string') {
    switch (kind) {
      case 'string':
      case 'enum':
        return '';
      case 'int':
      case 'float':
        return 0;
      case 'bool':
        return false;
      case 'bytes':
        return new Uint8Array(0);
      case 'list':
        return [];
      case 'map':
        return {};
      case 'ref':
        return null;
      default:
        return null;
    }
  }
  return defaultForType((field as any).type ?? 'string');
}

/** Get default value for a field type string. */
function defaultForType(typeStr: string): unknown {
  switch (typeStr) {
    case 'str':
    case 'string':
      return '';
    case 'int':
    case 'int32':
    case 'int64':
    case 'float':
    case 'float32':
    case 'float64':
    case 'number':
      return 0;
    case 'bool':
    case 'boolean':
      return false;
    case 'bytes':
      return new Uint8Array(0);
    default:
      if (typeStr.startsWith('list[') || typeStr.startsWith('List[')) return [];
      if (typeStr.startsWith('dict[') || typeStr.startsWith('Dict[') || typeStr.startsWith('Map[')) return {};
      return null;
  }
}
