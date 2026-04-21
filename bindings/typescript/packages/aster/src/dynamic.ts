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
import type {
  CanonicalFieldDef,
  CanonicalTypeDef,
} from './contract/identity.js';
import {
  ContainerKind,
  TypeKind,
} from './contract/identity.js';

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

  /**
   * Register Fory types by walking the canonical TypeDef graph. This is
   * the high-fidelity path used when the client has fetched the full
   * contract collection (`types/{hash}.bin` blobs) and decoded them via
   * `decodeTypeDefBytes`. Unlike `registerWithFory`, this walks
   * transitively — nested user types in `REF` fields are resolved via
   * the typeDef graph and registered recursively.
   *
   * @param rootWireTags wire tags to start the walk from (method
   *   request/response types).
   * @param typeDefs two lookup views of the same set of TypeDefs: by
   *   `{package}/{name}` tag, and by hex BLAKE3 hash. Callers build both
   *   once per contract and pass them in.
   * @returns the set of wire tags that were resolved via the typeDef
   *   graph. Callers can diff against `rootWireTags` to decide whether
   *   to fall back to the flat manifest path for unresolved roots.
   */
  registerFromTypeDefs(
    rootWireTags: readonly string[],
    typeDefs: {
      byTag: ReadonlyMap<string, CanonicalTypeDef>;
      byHash: ReadonlyMap<string, CanonicalTypeDef>;
    },
    Type: ForyTypeNamespace,
    codec: { registerType(typeInfo: unknown): void },
  ): Set<string> {
    const resolved = new Set<string>();
    const ordered = topoSortReachable(rootWireTags, typeDefs);
    // typesByTag carries the in-progress Fory struct typeInfo for each
    // already-registered tag, so nested REF fields can reference the
    // real typeInfo object (not a name placeholder) when building the
    // parent struct. Mirrors the `typesByTag.get(tag)` pattern in the
    // scanner-generated `BUILD_ALL_TYPES` body.
    const typesByTag = new Map<string, any>();

    for (const tag of ordered) {
      if (this.foryRegistered.has(tag)) {
        resolved.add(tag);
        continue;
      }
      const td = typeDefs.byTag.get(tag);
      if (!td) continue;

      const cls = this.getOrCreateFromTypeDef(tag, td);

      const foryFields: Record<string, unknown> = {};
      for (const f of td.fields) {
        foryFields[f.name] = foryFieldTypeFromCanonical(f, Type, typeDefs, typesByTag);
      }

      const { namespace, typeName } = splitWireTag(tag);
      const typeStruct = Type.struct(
        { namespace, typeName },
        foryFields,
        { withConstructor: true },
      );
      (typeStruct as { initMeta: (ctor: unknown) => void }).initMeta(cls);
      codec.registerType(typeStruct);
      this.foryRegistered.add(tag);
      typesByTag.set(tag, typeStruct);
      resolved.add(tag);
    }
    return resolved;
  }

  private getOrCreateFromTypeDef(tag: string, td: CanonicalTypeDef): DynamicType {
    let type = this.types.get(tag);
    if (!type) {
      type = createDynamicType(tag, td.fields.map(canonicalToManifestField));
      this.types.set(tag, type);
    }
    return type;
  }
}

// ── CanonicalTypeDef-driven helpers ─────────────────────────────────────────

interface ForyTypeNamespace {
  struct(opts: unknown, fields: unknown, meta?: unknown): any;
  string(): any;
  bool(): any;
  int8(): any;
  int16(): any;
  int32(): any;
  int64(): any;
  varInt32(): any;
  varInt64(): any;
  varUInt32(): any;
  varUInt64(): any;
  float32(): any;
  float64(): any;
  binary(): any;
  array(element: any): any;
  set(element: any): any;
  map(key: any, value: any): any;
  optional?(inner: any): any;
}

// Canonical ``type_primitive`` -> TS Fory Type factory. Keys MUST match
// the Fory xlang type mapping spec byte-for-byte
// (docs/specification/xlang_type_mapping.md). Notably "int32" is fixed
// 4 bytes (Type.int32()) while "varint32" is zigzag varint
// (Type.varInt32()) -- these are distinct type ids on the wire, and
// pyfory emits pyfory.int32 as spec "varint32" (NOT "int32"). Mapping
// both to Type.int32() here would cause a cross-binding length
// mismatch: pyfory writes 1 byte for small ints while TS would expect
// 4, so the reader runs off the payload with "Out of bounds access".
const PRIMITIVE_TO_FORY: Record<string, (T: ForyTypeNamespace) => any> = {
  bool: T => T.bool(),
  string: T => T.string(),
  binary: T => T.binary(),
  int8: T => T.int8(),
  int16: T => T.int16(),
  int32: T => T.int32(),
  int64: T => T.int64(),
  varint32: T => T.varInt32(),
  varint64: T => T.varInt64(),
  uint8: T => T.int8(),
  uint16: T => T.int16(),
  uint32: T => T.int32(),
  uint64: T => T.int64(),
  var_uint32: T => T.varUInt32(),
  var_uint64: T => T.varUInt64(),
  float32: T => T.float32(),
  float64: T => T.float64(),
  // Timestamps are carried as varint64 millis in pyfory's xlang.
  timestamp: T => T.varInt64(),
  uuid: T => T.string(),
};

function foryPrimitiveForCanonical(primName: string, Type: ForyTypeNamespace): any {
  const fn = PRIMITIVE_TO_FORY[primName];
  if (fn) return fn(Type);
  // Unknown primitive name — fall back to string so decode surfaces a
  // stringified value rather than crashing. The server-side Fory hash
  // check would reject a mismatched type anyway; we never silently
  // encode wrong bytes.
  return Type.string();
}

/**
 * Topologically sort the set of TypeDefs reachable from `roots`, leaves
 * first. Used by `registerFromTypeDefs` so nested struct types are
 * registered before parents reference them — matches the ordering
 * emitted by the `aster-gen` scanner into `WIRE_TYPES`.
 *
 * Back-edges (cycles via SELF_REF) are broken by visit order: a node
 * that re-enters the DFS while already on the stack is ignored as a
 * self-reference, exactly like the scanner's SCC handling.
 */
function topoSortReachable(
  roots: readonly string[],
  typeDefs: { byTag: ReadonlyMap<string, CanonicalTypeDef>; byHash: ReadonlyMap<string, CanonicalTypeDef> },
): string[] {
  const ordered: string[] = [];
  const visited = new Set<string>();
  const onStack = new Set<string>();

  const refTag = (hashHex: string): string | undefined => {
    const nested = typeDefs.byHash.get(hashHex);
    return nested ? `${nested.package}/${nested.name}` : undefined;
  };

  const collectChildren = (td: CanonicalTypeDef): string[] => {
    const out: string[] = [];
    for (const f of td.fields) {
      if (f.typeKind === TypeKind.REF) {
        const tag = refTag(f.typeRef);
        if (tag) out.push(tag);
      }
      if (f.container !== ContainerKind.NONE && f.containerKeyKind === TypeKind.REF) {
        const tag = refTag(f.containerKeyRef);
        if (tag) out.push(tag);
      }
    }
    return out;
  };

  const visit = (tag: string): void => {
    if (visited.has(tag) || onStack.has(tag)) return;
    const td = typeDefs.byTag.get(tag);
    if (!td) return;
    onStack.add(tag);
    for (const child of collectChildren(td)) visit(child);
    onStack.delete(tag);
    visited.add(tag);
    ordered.push(tag);
  };

  for (const r of roots) visit(r);
  return ordered;
}

function foryFieldTypeFromCanonical(
  f: CanonicalFieldDef,
  Type: ForyTypeNamespace,
  typeDefs: { byTag: ReadonlyMap<string, CanonicalTypeDef>; byHash: ReadonlyMap<string, CanonicalTypeDef> },
  typesByTag: ReadonlyMap<string, any>,
): any {
  const wrap = (inner: any) =>
    f.optional && typeof Type.optional === 'function' ? Type.optional(inner) : inner;

  const resolveRef = (typeRefHex: string): any | undefined => {
    const nested = typeDefs.byHash.get(typeRefHex);
    if (!nested) return undefined;
    const nestedTag = `${nested.package}/${nested.name}`;
    return typesByTag.get(nestedTag);
  };

  const resolveLeaf = (kind: number, primitive: string, typeRefHex: string): any => {
    if (kind === TypeKind.PRIMITIVE) return foryPrimitiveForCanonical(primitive, Type);
    if (kind === TypeKind.REF) {
      const ref = resolveRef(typeRefHex);
      if (ref) return ref;
      // Nested struct wasn't registered yet — topological sort should
      // prevent this, so reaching here means either a missing TypeDef
      // blob or a cycle not caught by SELF_REF. Fall back to string.
      return Type.string();
    }
    return Type.string();
  };

  if (f.container === ContainerKind.LIST || f.container === ContainerKind.SET) {
    const element = resolveLeaf(f.typeKind, f.typePrimitive, f.typeRef);
    const container = f.container === ContainerKind.SET
      ? Type.set(element)
      : Type.array(element);
    return wrap(container);
  }
  if (f.container === ContainerKind.MAP) {
    const keyType = resolveLeaf(f.containerKeyKind, f.containerKeyPrimitive, f.containerKeyRef);
    const valType = resolveLeaf(f.typeKind, f.typePrimitive, f.typeRef);
    return wrap(Type.map(keyType, valType));
  }
  if (f.typeKind === TypeKind.REF) {
    const ref = resolveRef(f.typeRef);
    return wrap(ref ?? Type.string());
  }
  if (f.typeKind === TypeKind.PRIMITIVE) {
    return wrap(foryPrimitiveForCanonical(f.typePrimitive, Type));
  }
  return wrap(Type.string());
}

function canonicalToManifestField(f: CanonicalFieldDef): ManifestField {
  // `defaultForField` branches on `kind` to produce container defaults
  // (empty list / empty Map). Without a correct `kind` here, container
  // fields fall through to `null`, which Fory then rejects at encode
  // time as "field X is not nullable". We also emit a legacy `type`
  // string so flat-manifest consumers still work.
  let kind: 'string' | 'int' | 'float' | 'bool' | 'bytes' | 'list' | 'map' | 'ref';
  let legacyType: string;
  if (f.container === ContainerKind.LIST || f.container === ContainerKind.SET) {
    kind = 'list';
    legacyType = 'list';
  } else if (f.container === ContainerKind.MAP) {
    kind = 'map';
    legacyType = 'map';
  } else if (f.typeKind === TypeKind.PRIMITIVE) {
    kind = primitiveToManifestKind(f.typePrimitive);
    legacyType = f.typePrimitive;
  } else if (f.typeKind === TypeKind.REF) {
    kind = 'ref';
    legacyType = 'ref';
  } else {
    kind = 'string';
    legacyType = 'string';
  }
  const field: ManifestField & { kind: typeof kind } = {
    name: f.name,
    type: legacyType,
    required: f.required,
    default: undefined,
    kind,
  };
  return field;
}

function primitiveToManifestKind(
  primName: string,
): 'string' | 'int' | 'float' | 'bool' | 'bytes' {
  switch (primName) {
    case 'string':
    case 'uuid':
      return 'string';
    case 'bool':
      return 'bool';
    case 'binary':
      return 'bytes';
    case 'float32':
    case 'float64':
      return 'float';
    default:
      // int8/16/32/64, uint8/16/32/64, timestamp, etc. all default to
      // int — defaultForField only needs a numeric default here.
      return 'int';
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
        // Fory's xlang map serializer calls `.entries()` and `.size`,
        // which only exist on real Map objects. Plain `{}` here produces
        // "v.tags.entries is not a function" at encode time.
        return new Map();
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
      if (typeStr.startsWith('dict[') || typeStr.startsWith('Dict[') || typeStr.startsWith('Map[')) return new Map();
      return null;
  }
}
