/**
 * Payload codec -- Fory XLANG serialization of call payloads.
 *
 * Spec reference: Aster-SPEC §5.1 (serialization). Compression (§5.6) is a
 * framing concern handled by Rust core (`framing.ts` -> `encodePayloadFrame`
 * / `payloadOf`), and the RPC envelope is AEv1 (`protocol.ts`), so a codec
 * here only turns payload objects into Fory bytes and back.
 *
 * JSON serialization (mode 3) is retired across all bindings (Aster-SPEC
 * §5.1); there is no JSON codec.
 */

import { WIRE_TYPE_KEY } from './decorators.js';

/** Default compression threshold in bytes (4 KiB). Payloads strictly larger are zstd-compressed by core. */
export const DEFAULT_COMPRESSION_THRESHOLD = 4096;

/** Re-export WIRE_TYPE_KEY as wireType for API compatibility. */
export { WIRE_TYPE_KEY as wireType };

/** Get the wire type tag from a @WireType-decorated class. */
export function getWireType(cls: unknown): string | undefined {
  return (cls as any)?.[WIRE_TYPE_KEY];
}

/** Payload codec interface: objects to wire bytes and back. */
export interface Codec {
  encode(obj: unknown, hintType?: unknown): Uint8Array;
  decode(payload: Uint8Array, hintType?: unknown): unknown;
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

// -- ForyCodec ----------------------------------------------------------------

/**
 * ForyCodec -- cross-language payload serialization via `@apache-fory/core`.
 *
 * Build it over a runtime from {@link getXlangForyAndType} /
 * {@link createXlangCodec} so the Fory options match every other binding
 * (xlang, compatible, ref tracking off -- see `xlang.ts`).
 *
 * @example
 * ```ts
 * const codec = createXlangCodec();
 * codec.registerType(myStructTypeInfo);
 * const bytes = codec.encode(new MyStruct({ ... }));
 * ```
 */
export class ForyCodec implements Codec {
  readonly fory: any;
  private serializers = new Map<string, { serialize: any; deserialize: any }>();

  constructor(foryInstance: any) {
    this.fory = foryInstance;
  }

  /**
   * Register a struct type for serialization.
   *
   * If the typeInfo has been linked to a class via `initMeta(cls)`, the
   * class is registered, so Fory's generated deserializer constructs
   * instances of it; otherwise the typeInfo itself is registered.
   */
  registerType(typeInfo: any): void {
    const creator = typeInfo?.options?.creator;
    const arg = typeof creator === 'function' ? creator : typeInfo;
    const { serialize, deserialize } = this.fory.register(arg);
    const ns: string = typeInfo?.namespace ?? '';
    const name: string = typeInfo?.typeName ?? String(typeInfo);
    this.serializers.set(ns ? `${ns}/${name}` : name, { serialize, deserialize });
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
    // Convert plain object literals to typed instances when a hint type is
    // provided: `{ agent_id: 'x' }` becomes `new StatusRequest({ ... })` so
    // Fory serializes it under the registered wire type.
    if (hintType && typeof hintType === 'function' && obj !== null && typeof obj === 'object'
        && !Array.isArray(obj) && !(obj instanceof (hintType as any))) {
      const ctor = hintType as new (...args: any[]) => any;
      obj = new ctor(obj);
    }
    const result = this.fory.serialize(obj);
    return new Uint8Array(result);
  }

  decode(payload: Uint8Array, _hintType?: unknown): unknown {
    return this.fory.deserialize(payload);
  }

  /** Registered wire tags (`namespace/TypeName`). */
  registeredTypes(): string[] {
    return [...this.serializers.keys()];
  }
}
