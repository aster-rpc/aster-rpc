/**
 * Types and registration glue for files emitted by `aster-gen`.
 *
 * `aster-gen` scans a TypeScript project at build time and emits a
 * single `rpc.generated.ts` file exporting `SERVICES` and `WIRE_TYPES`
 * literals that describe every `@Service` and `@WireType` class found.
 * The runtime decorators are pure markers — all the metadata lives in
 * the generated file.
 *
 * The generated file is consumed via {@link registerGenerated}:
 *
 * ```ts
 * import { AsterServer, registerGenerated } from '@aster-rpc/aster';
 * import { SERVICES, WIRE_TYPES } from './rpc.generated.js';
 *
 * registerGenerated({ SERVICES, WIRE_TYPES });
 * const server = new AsterServer({ services: [new MissionControlService()] });
 * ```
 *
 * `registerGenerated` stamps the generated metadata onto each class
 * constructor under the existing `SERVICE_INFO_KEY` / `WIRE_TYPE_KEY`
 * symbols, so the runtime path (`ServiceRegistry.register`, the JSON
 * shape validator, Fory type registration) keeps working unchanged.
 */

import type { SerializationMode } from './types.js';
import { RpcPattern } from './types.js';
import type { Metadata } from './metadata.js';
import {
  SERVICE_INFO_KEY,
  type ServiceInfo,
  type MethodInfo,
  type CapabilityRequirement,
} from './service.js';
import { WIRE_TYPE_KEY } from './decorators.js';
import type { ManifestField } from './contract/manifest.js';

/**
 * Canonical wire type tokens recognized by the scanner.
 *
 * Mirrors the wire-type set in `ffi_spec/Aster-ContractIdentity.md`
 * §11.3.2.3. Container types (`list<T>`, `map<K,V>`, `set<T>`,
 * `nullable<T>`) are represented via the {@link WireFieldShape.container}
 * discriminator rather than embedded in a string, so readers don't have
 * to parse.
 */
export type WirePrimitive =
  | 'bool'
  | 'int8' | 'int16' | 'int32' | 'int64'
  | 'uint8' | 'uint16' | 'uint32' | 'uint64'
  | 'float32' | 'float64'
  | 'string' | 'binary'
  | 'timestamp' | 'uuid';

/**
 * A single field on a `@WireType` class, as the scanner sees it.
 *
 * `kind` discriminates between primitives, references to other
 * `@WireType` classes (by tag), and container types. Nullable wraps
 * any of these.
 */
export type WireFieldShape =
  | { name: string; kind: 'primitive'; wire: WirePrimitive; nullable: boolean }
  | { name: string; kind: 'ref'; refTag: string; nullable: boolean }
  | { name: string; kind: 'list'; element: WireFieldShape; nullable: boolean }
  | { name: string; kind: 'set'; element: WireFieldShape; nullable: boolean }
  | { name: string; kind: 'map'; key: WireFieldShape; value: WireFieldShape; nullable: boolean }
  | { name: string; kind: 'enum'; refTag: string; nullable: boolean };

/**
 * A `@WireType` class as described in the generated file.
 *
 * The scanner emits one entry per class, in dependency order (leaves
 * first) so Fory type registration respects forward references.
 * `foryTypeInfo` is the pre-built object passed to
 * `fory.registerSerializer(...)` — users never write it themselves.
 */
export interface WireTypeShape {
  /** Constructor of the `@WireType`-decorated class. */
  ctor: new (...args: any[]) => any;
  /** The string tag from `@WireType(tag)`. */
  tag: string;
  /** Ordered field list from the class body (declaration order). */
  fields: readonly WireFieldShape[];
  /** Fast-lookup field name set, for the JSON shape validator. */
  fieldNameSet: ReadonlySet<string>;
  /** field name -> nested `@WireType` class, for recursive JSON validation. */
  nestedTypes: ReadonlyMap<string, new (...args: any[]) => any>;
  /** field name -> list-element `@WireType` class, for recursive JSON validation. */
  elementTypes: ReadonlyMap<string, new (...args: any[]) => any>;
  /**
   * Pre-built Fory typeInfo object, ready to pass to
   * `fory.registerSerializer(...)`. Opaque to the runtime — only
   * ForyCodec understands its shape.
   */
  foryTypeInfo: unknown;
}

/**
 * A method on a `@Service` class, as the scanner saw it.
 *
 * All of `requestType`, `responseType`, and `acceptsCtx` come from the
 * AST — not from runtime introspection. `acceptsCtx` is true iff the
 * method declares a second parameter whose type is `CallContext`.
 */
export interface GeneratedMethodDef {
  name: string;
  pattern: RpcPattern;
  requestType: (new (...args: any[]) => any) | undefined;
  responseType: (new (...args: any[]) => any) | undefined;
  acceptsCtx: boolean;
  idempotent: boolean;
  timeout: number | undefined;
  serialization: SerializationMode | undefined;
  requires: CapabilityRequirement | undefined;
  metadata: Metadata | undefined;
  /**
   * Pre-derived manifest fields for the request type. Replaces the
   * runtime `new Ctor()` + `Object.keys` introspection path in
   * `_buildManifest` — handles empty arrays, nullable nested types,
   * and non-default-constructible classes without instantiating.
   */
  requestFields: readonly ManifestField[];
  /** Same as `requestFields`, for the response type. */
  responseFields: readonly ManifestField[];
}

/**
 * A `@Service` class as described in the generated file.
 *
 * `methods` is an ordered list (not yet a Map) so the generated
 * literal stays readable. {@link registerGenerated} converts it to
 * the Map shape the runtime dispatch path expects.
 */
export interface GeneratedServiceDef {
  ctor: new (...args: any[]) => any;
  name: string;
  version: number;
  scoped: 'shared' | 'session';
  serializationModes: readonly SerializationMode[];
  requires: CapabilityRequirement | undefined;
  metadata: Metadata | undefined;
  methods: readonly GeneratedMethodDef[];
}

/**
 * Registry of generated method-level field info keyed by `{serviceName, version, methodName}`.
 *
 * Consumed by `runtime.ts:_buildManifest` in preference to the
 * runtime `extractFields` reflection path. Populated by
 * {@link registerGenerated}.
 */
const _methodFieldsRegistry = new Map<string, { requestFields: readonly ManifestField[]; responseFields: readonly ManifestField[] }>();

function methodFieldsKey(service: string, version: number, method: string): string {
  return `${service}/${version}/${method}`;
}

/** Look up pre-derived manifest fields for a method. Returns `undefined` if the project was not generated. */
export function getGeneratedMethodFields(
  service: string,
  version: number,
  method: string,
): { requestFields: readonly ManifestField[]; responseFields: readonly ManifestField[] } | undefined {
  return _methodFieldsRegistry.get(methodFieldsKey(service, version, method));
}

/**
 * Registry of `WireTypeShape` entries keyed by class constructor.
 *
 * The JSON shape validator in `codec.ts` consults this to look up the
 * pre-built field set / nested type map without instantiating the
 * class. Populated by {@link registerGenerated}.
 */
const _wireShapeRegistry = new WeakMap<new (...args: any[]) => any, WireTypeShape>();

/** Look up the pre-built shape for a `@WireType` class. */
export function getWireShape(
  cls: new (...args: any[]) => any,
): WireTypeShape | undefined {
  return _wireShapeRegistry.get(cls);
}

/**
 * Minimal codec surface `registerGenerated` uses to register Fory
 * type infos. Matches `ForyCodec.registerType` without importing the
 * class directly (avoids a circular module dep).
 */
export interface GeneratedCodec {
  registerType(typeInfo: unknown): void;
}

/** Options for {@link registerGenerated}. */
export interface RegisterGeneratedOptions {
  /** SERVICES export from `rpc.generated.ts`. */
  SERVICES: readonly GeneratedServiceDef[];
  /** WIRE_TYPES export from `rpc.generated.ts`. */
  WIRE_TYPES: readonly WireTypeShape[];
  /**
   * Optional Fory codec. When provided, every wire type is
   * registered with Fory in dependency order (leaves first). For
   * JSON-only services this can be omitted.
   */
  codec?: GeneratedCodec;
}

/**
 * Wire the generated file into the runtime.
 *
 * Call this once, before constructing `AsterServer`. It:
 *
 * 1. Stamps `WIRE_TYPE_KEY` onto every wire type class constructor
 *    and records its pre-built shape in the shape registry so the
 *    JSON validator doesn't `new cls()` at decode time.
 * 2. If a codec is provided, registers every wire type with Fory in
 *    dependency order.
 * 3. For every service, builds a `ServiceInfo` (the same shape the
 *    old `@Service` decorator used to produce), attaches handler
 *    references from the class prototype, and stamps it onto the
 *    class constructor under `SERVICE_INFO_KEY`. The existing
 *    `ServiceRegistry.register(instance)` path then finds it
 *    unchanged.
 *
 * Calling this more than once is safe — later calls overwrite any
 * previous registration for the same class.
 */
export function registerGenerated(opts: RegisterGeneratedOptions): void {
  // Phase 1: wire types — shape registry + WIRE_TYPE_KEY + optional Fory.
  for (const shape of opts.WIRE_TYPES) {
    (shape.ctor as any)[WIRE_TYPE_KEY] = shape.tag;
    _wireShapeRegistry.set(shape.ctor, shape);
  }
  if (opts.codec) {
    for (const shape of opts.WIRE_TYPES) {
      // `foryTypeInfo` is null in v1 generated files — the Fory JS
      // API currently needs a user-supplied `buildTypeInfo` callback
      // (see `ForyCodec.registerTypeGraph`). Skip when null so users
      // can still opt into the generated ServiceInfo path while
      // feeding Fory via the existing runtime path.
      if (shape.foryTypeInfo != null) {
        opts.codec.registerType(shape.foryTypeInfo);
      }
    }
  }

  // Phase 2: services — build ServiceInfo per class and stamp it on
  // the constructor under SERVICE_INFO_KEY, matching the legacy
  // decoration output so `getServiceInfo` / `ServiceRegistry.register`
  // keep working. Also stash the pre-derived manifest field info so
  // `runtime.ts:_buildManifest` can skip its runtime `new Ctor()` path.
  for (const svc of opts.SERVICES) {
    const proto = svc.ctor.prototype;
    const methods = new Map<string, MethodInfo>();
    for (const m of svc.methods) {
      const handler = proto[m.name];
      if (typeof handler !== 'function') {
        throw new Error(
          `registerGenerated: ${svc.ctor.name}.${m.name} is not a function. ` +
          `The generated file references a method that no longer exists on the class — ` +
          `regenerate with: bunx aster-gen`,
        );
      }
      methods.set(m.name, {
        name: m.name,
        pattern: m.pattern,
        requestType: m.requestType,
        responseType: m.responseType,
        timeout: m.timeout,
        idempotent: m.idempotent,
        serialization: m.serialization,
        requires: m.requires,
        handler,
        metadata: m.metadata,
        acceptsCtx: m.acceptsCtx,
      });
      _methodFieldsRegistry.set(
        methodFieldsKey(svc.name, svc.version, m.name),
        { requestFields: m.requestFields, responseFields: m.responseFields },
      );
    }
    const info: ServiceInfo = {
      name: svc.name,
      version: svc.version,
      scoped: svc.scoped,
      methods,
      serializationModes: [...svc.serializationModes],
      requires: svc.requires,
      metadata: svc.metadata,
      instance: undefined,
    };
    (svc.ctor as any)[SERVICE_INFO_KEY] = info;
  }
}
