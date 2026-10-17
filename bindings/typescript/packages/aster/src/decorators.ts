/**
 * Service and method decorators for defining Aster RPC services.
 *
 * Spec reference: S7.1-7.4 (decorators), S7.6 (language ownership).
 *
 * Uses TC39 Stage 3 decorators (TS 5.0+). These compile away during
 * TypeScript compilation — no runtime decorator support needed.
 *
 * **Build-time codegen.** The scanner CLI `aster-gen` (ships in this
 * package as a `bin`) walks your `tsconfig.json` at build time and
 * emits `aster-rpc.generated.ts` carrying full service metadata
 * derived from AST types — no runtime reflection, no manual
 * `{ request, response }` options. `AsterServer.start()` auto-imports
 * the generated file. See the README for the full workflow.
 *
 * @example
 * ```ts
 * @Service({ name: "Echo", version: 1 })
 * class EchoService {
 *   @Rpc({ timeout: 30 })
 *   async echo(req: EchoRequest): Promise<EchoResponse> {
 *     return new EchoResponse({ reply: req.message });
 *   }
 *
 *   @ServerStream()
 *   async *watchItems(req: WatchRequest): AsyncGenerator<ItemUpdate> {
 *     yield new ItemUpdate({ item: "one" });
 *   }
 * }
 * ```
 */

import { RpcPattern } from './types.js';
import type { SerializationMode } from './types.js';
import type { Metadata } from './metadata.js';
import {
  SERVICE_INFO_KEY,
  METHOD_INFO_KEY,
  type ServiceInfo,
  type MethodInfo,
  type CapabilityRequirement,
} from './service.js';

// -- Service decorator --------------------------------------------------------

export interface ServiceOptions {
  name: string;
  version?: number;
  /** Dispatch scope. Canonical values are 'shared' and 'session'.
   *  The legacy alias 'stream' is still accepted on input. */
  scoped?: 'shared' | 'session' | 'stream';
  serialization?: SerializationMode[];
  requires?: CapabilityRequirement;
  metadata?: Metadata;
}

/**
 * Decorate a class as an Aster RPC service.
 *
 * @example
 * ```ts
 * @Service({ name: "Echo", version: 1 })
 * class EchoService { ... }
 * ```
 */
export function Service(options: ServiceOptions) {
  return function <T extends new (...args: any[]) => any>(
    target: T,
    _context?: ClassDecoratorContext,
  ): T {
    // Collect methods that were decorated with @Rpc etc.
    const methods = new Map<string, MethodInfo>();

    // Scan prototype for method decorators
    const proto = target.prototype;
    const methodNames = Object.getOwnPropertyNames(proto).filter(
      (n) => n !== 'constructor',
    );
    for (const name of methodNames) {
      const descriptor = Object.getOwnPropertyDescriptor(proto, name);
      if (!descriptor || typeof descriptor.value !== 'function') continue;
      const info: MethodInfo | undefined = descriptor.value[METHOD_INFO_KEY];
      if (info) {
        // Use explicit name from @Rpc({name: "..."}) if set, else method name
        if (!info.name) {
          info.name = name;
        }
        info.handler = descriptor.value;
        // TypeScript erases types at runtime; we use Function.length as
        // the signal for "handler wants a CallContext". A handler that
        // declares more than one positional parameter gets the ctx
        // injected as the second argument by the server dispatch.
        info.acceptsCtx = (descriptor.value as Function).length > 1;
        methods.set(info.name, info);
      }
    }

    // Canonical scope value is 'session'; accept the legacy 'stream' alias
    // on input.
    const scoped = options.scoped === 'stream' ? 'session' : (options.scoped ?? 'shared');

    const serviceInfo: ServiceInfo = {
      name: options.name,
      version: options.version ?? 1,
      scoped,
      methods,
      serializationModes: options.serialization ?? [],
      requires: options.requires,
      metadata: options.metadata,
      instance: undefined,
    };

    (target as any)[SERVICE_INFO_KEY] = serviceInfo;
    return target;
  };
}

// -- Method decorators --------------------------------------------------------

interface RpcOptions {
  /** Override wire name (defaults to the method name). */
  name?: string;
  timeout?: number;
  idempotent?: boolean;
  serialization?: SerializationMode;
  requires?: CapabilityRequirement | string;
  metadata?: Metadata;
  /**
   * **Legacy.** Request message constructor. Before `aster-gen`,
   * this was required so the manifest publisher and `gen-client`
   * could reach the type despite erasure. With `aster-gen` the
   * scanner reads the first parameter type from the AST — omit
   * this option. Still honored as a fallback when the scanner
   * hasn't been run; see `ffi_spec/ts-buildtime-audit.md`.
   *
   * @deprecated Run `bunx aster-gen` and call `registerGenerated()`
   *   instead.
   */
  request?: new (...args: any[]) => any;
  /**
   * **Legacy.** Response message constructor. See {@link request}.
   *
   * @deprecated Run `bunx aster-gen` and call `registerGenerated()`
   *   instead.
   */
  response?: new (...args: any[]) => any;
}

function methodDecorator(pattern: RpcPattern, options?: RpcOptions) {
  // Dual-mode: handles both TC39 Stage-3 and TS/Bun experimental
  // ("Stage-1") method decorator call shapes.
  //
  // Stage 3 (TS 5.0+ default): `(value, context)`; `value` is the
  // method function, `context` is `{ kind, name, ... }`.
  // Experimental / Stage 1 (still the only mode bun >=1.2 implements
  // for .ts files today, regardless of `experimentalDecorators`):
  // `(target, propertyKey, descriptor)`; `target` is the prototype,
  // `propertyKey` is a string/symbol, `descriptor` is the property
  // descriptor whose `.value` is the method function.
  //
  // In both shapes we stamp `METHOD_INFO_KEY` on the method FUNCTION
  // so `@Service`'s prototype scan can pick it up identically.
  return function (this: unknown, ...args: any[]): any {
    const info: MethodInfo = {
      name: options?.name ?? '',
      pattern,
      requestType: options?.request,
      responseType: options?.response,
      timeout: options?.timeout,
      idempotent: options?.idempotent ?? false,
      serialization: options?.serialization,
      requires: options?.requires as CapabilityRequirement | undefined,
      handler: undefined,
      metadata: options?.metadata,
    };

    // Stage-1 shape: 3 args, second is property key (string|symbol),
    // third is a property descriptor with a `.value` method function.
    if (
      args.length >= 3 &&
      (typeof args[1] === 'string' || typeof args[1] === 'symbol') &&
      args[2] &&
      typeof args[2] === 'object' &&
      'value' in args[2]
    ) {
      const descriptor = args[2] as PropertyDescriptor;
      const method = descriptor.value;
      if (typeof method === 'function') {
        (method as any)[METHOD_INFO_KEY] = info;
      }
      return descriptor;
    }

    // Stage-3 shape: first arg is the method function; stamp directly.
    const target = args[0];
    (target as any)[METHOD_INFO_KEY] = info;
    return target;
  };
}

/**
 * Mark a method as a unary RPC (single request, single response).
 *
 * @example
 * ```ts
 * @Rpc({ timeout: 30, idempotent: true })
 * async getUser(req: GetUserRequest): Promise<User> { ... }
 * ```
 */
export function Rpc(options?: RpcOptions) {
  return methodDecorator(RpcPattern.UNARY, options);
}

/**
 * Mark a method as a server-streaming RPC (single request, multiple responses).
 * Method must be an async generator.
 *
 * @example
 * ```ts
 * @ServerStream()
 * async *watchUpdates(req: WatchRequest): AsyncGenerator<Update> { ... }
 * ```
 */
export function ServerStream(options?: RpcOptions) {
  return methodDecorator(RpcPattern.SERVER_STREAM, options);
}

/**
 * Mark a method as a client-streaming RPC (multiple requests, single response).
 *
 * @example
 * ```ts
 * @ClientStream()
 * async uploadBatch(requests: AsyncIterable<Item>): Promise<BatchResult> { ... }
 * ```
 */
export function ClientStream(options?: RpcOptions) {
  return methodDecorator(RpcPattern.CLIENT_STREAM, options);
}

/**
 * Mark a method as a bidirectional-streaming RPC.
 *
 * @example
 * ```ts
 * @BidiStream()
 * async *chat(requests: AsyncIterable<Message>): AsyncGenerator<Message> { ... }
 * ```
 */
export function BidiStream(options?: RpcOptions) {
  return methodDecorator(RpcPattern.BIDI_STREAM, options);
}

// -- Wire type decorator ------------------------------------------------------

/** Metadata key for wire type tag. */
export const WIRE_TYPE_KEY = Symbol.for('aster.wire_type');

/** Metadata key for wire type field metadata. */
export const WIRE_TYPE_FIELDS_KEY = Symbol.for('aster.wire_type_fields');

/**
 * Options for the `WireType` decorator.
 * @group Decorators
 */
export interface WireTypeOptions {
  /** Field-level metadata (field name -> Metadata). */
  metadata?: Record<string, Metadata>;
}

/**
 * Register a class as a Fory XLANG wire type.
 *
 * @example
 * ```ts
 * @WireType("billing/Invoice")
 * class Invoice {
 *   amount = 0;
 *   currency = "USD";
 * }
 *
 * @WireType("billing/Invoice", {
 *   metadata: { amount: new Metadata({ description: "Total in cents" }) }
 * })
 * class InvoiceWithDocs {
 *   amount = 0;
 *   currency = "USD";
 * }
 * ```
 */
export function WireType(tag: string, options?: WireTypeOptions) {
  return function <T extends new (...args: any[]) => any>(
    target: T,
    context: ClassDecoratorContext,
  ): T {
    (target as any)[WIRE_TYPE_KEY] = tag;
    if (options?.metadata) {
      (target as any)[WIRE_TYPE_FIELDS_KEY] = options.metadata;
    }
    const defaults = (context?.metadata as any)?.[CONTRACT_DEFAULTS_KEY];
    if (defaults) {
      Object.defineProperty(target, CONTRACT_DEFAULTS_KEY, { value: { ...defaults } });
    }
    return target;
  };
}

// -- Contract defaults ------------------------------------------------------------

// TC39 decorator metadata needs `Symbol.metadata`; Node does not ship it yet.
// `Symbol.for('Symbol.metadata')` is the shared fallback that TypeScript- and
// esbuild-compiled decorators both pick up.
(Symbol as any).metadata ??= Symbol.for('Symbol.metadata');

/** Static key holding a `@WireType` class's declared contract defaults. */
export const CONTRACT_DEFAULTS_KEY = Symbol.for('aster.contract_defaults');

/** Marker for `@ContractDefault()` without an argument (default = initializer). */
const FROM_INITIALIZER = Symbol.for('aster.contract_default.initializer');

/**
 * Declare a field's **contract default** (Aster-ContractIdentity §11.3.2.3).
 *
 * Every TypeScript field needs an initializer, so a plain initializer is
 * *not* a contract default: such a field stays `required` in the contract.
 * Mark the fields whose default is part of the contract -- the value is then
 * baked into `contract_id` (`FieldDef.required = false`, `default_value` =
 * the canonical encoding), exactly as Rust's `#[aster(default = ...)]`.
 *
 * - `@ContractDefault()` -- the field's initializer is the default.
 * - `@ContractDefault(value)` -- `value` is the default (should equal the
 *   initializer).
 *
 * Allowed: `string`, `boolean`, `i32`/`i64`/`bigint`, `number` (float64),
 * nullable scalars (non-null default), and empty `[]` / `Set` / `Map` /
 * `Record`. Anything else fails at `aster-gen` time. Defaults never change
 * the Fory payload bytes. `aster-gen` reads these decorators statically,
 * so the value must be a literal.
 *
 * @example
 * ```ts
 * @WireType('defaults.v1/Settings')
 * class Settings {
 *   name = '';                          // required
 *   @ContractDefault() state = 'idle';  // default "idle"
 *   @ContractDefault() retries: i32 = i32(-3);
 *   @ContractDefault() tags: string[] = [];
 * }
 * ```
 * @group Decorators
 */
export function ContractDefault(...args: [] | [value: unknown]) {
  const declared = args.length > 0 ? args[0] : FROM_INITIALIZER;
  return function (_value: undefined, context: ClassFieldDecoratorContext): void {
    const meta = context.metadata as Record<PropertyKey, any> | undefined;
    if (!meta) return;
    const own = Object.prototype.hasOwnProperty.call(meta, CONTRACT_DEFAULTS_KEY)
      ? meta[CONTRACT_DEFAULTS_KEY]
      : (meta[CONTRACT_DEFAULTS_KEY] = { ...(meta[CONTRACT_DEFAULTS_KEY] ?? {}) });
    own[String(context.name)] = declared;
  };
}

/** Static key holding a `@WireType` class's `@WireName` overrides (`{ property: name }`). */
export const WIRE_NAMES_KEY = Symbol.for('aster.wire_names');

/**
 * A field's **contract name**, when the contract is defined in another
 * language (default: the property name, verbatim; `ffi_spec/Aster-FieldNaming.md`).
 *
 * Fory writes the property's own snake_case form on the wire, so the name's
 * snake_case form must equal the property's: `@WireName('written_by')` on
 * `writtenBy` is fine, on `author` it is refused. `aster-gen` reads this
 * decorator statically, so the name must be a string literal.
 *
 * @example
 * ```ts
 * @WireType('books.v1/Book')   // a contract defined in Rust
 * class Book {
 *   @WireName('written_by') writtenBy: Author | null = null;
 * }
 * ```
 * @group Decorators
 */
export function WireName(name: string) {
  // Dual-mode: TC39 field decorators get (undefined, context); Bun's legacy
  // decorators get (prototype, propertyKey).
  return function (target: unknown, context: ClassFieldDecoratorContext | string | symbol): void {
    let meta: Record<PropertyKey, any> | undefined;
    let key: string;
    if (typeof context === 'object' && context !== null) {
      meta = context.metadata as Record<PropertyKey, any> | undefined;
      key = String(context.name);
    } else {
      meta = (target as any)?.constructor;
      key = String(context);
    }
    if (!meta) return;
    const own = Object.prototype.hasOwnProperty.call(meta, WIRE_NAMES_KEY)
      ? meta[WIRE_NAMES_KEY]
      : (meta[WIRE_NAMES_KEY] = { ...(meta[WIRE_NAMES_KEY] ?? {}) });
    own[key] = name;
  };
}

/**
 * The contract defaults declared on a `@WireType` class, as
 * `{ fieldName: value }`. Fields declared with a bare `@ContractDefault()`
 * resolve to the value a default-constructed instance holds.
 */
export function getContractDefaults(cls: new (...args: any[]) => any): Record<string, unknown> {
  const raw: Record<string, unknown> | undefined =
    (cls as any)[CONTRACT_DEFAULTS_KEY] ?? (cls as any)[(Symbol as any).metadata]?.[CONTRACT_DEFAULTS_KEY];
  if (!raw) return {};
  const out: Record<string, unknown> = {};
  let instance: any;
  for (const [name, v] of Object.entries(raw)) {
    if (v === FROM_INITIALIZER) {
      instance ??= new cls();
      out[name] = instance[name];
    } else {
      out[name] = v;
    }
  }
  return out;
}
