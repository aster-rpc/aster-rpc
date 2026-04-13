/**
 * Service and method decorators for defining Aster RPC services.
 *
 * Spec reference: S7.1-7.4 (decorators), S7.6 (language ownership)
 *
 * Uses TC39 Stage 3 decorators (TS 5.0+). These compile away during
 * TypeScript compilation -- no runtime decorator support needed.
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
    _context: ClassDecoratorContext,
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
   * Request message constructor. TypeScript erases type parameters at runtime,
   * so the contract publisher and codegen pipeline cannot discover the request
   * shape unless it is passed explicitly here. Without it the published manifest
   * will lack the wire tag and field list, which breaks `aster gen-client`
   * for cross-language consumers.
   */
  request?: new (...args: any[]) => any;
  /** Response message constructor. See `request` for why this is needed. */
  response?: new (...args: any[]) => any;
}

function methodDecorator(pattern: RpcPattern, options?: RpcOptions) {
  return function <T extends (...args: any[]) => any>(
    target: T,
    _context: ClassMethodDecoratorContext,
  ): T {
    const info: MethodInfo = {
      name: options?.name ?? '', // explicit name or filled in by @Service
      pattern,
      requestType: options?.request,
      responseType: options?.response,
      timeout: options?.timeout,
      idempotent: options?.idempotent ?? false,
      serialization: options?.serialization,
      requires: options?.requires as CapabilityRequirement | undefined,
      handler: undefined, // filled in by @Service
      metadata: options?.metadata,
    };

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
    _context: ClassDecoratorContext,
  ): T {
    (target as any)[WIRE_TYPE_KEY] = tag;
    if (options?.metadata) {
      (target as any)[WIRE_TYPE_FIELDS_KEY] = options.metadata;
    }
    return target;
  };
}
