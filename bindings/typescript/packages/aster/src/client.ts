/**
 * Client stub generation.
 *
 * Creates a typed client proxy from a @Service-decorated class.
 * Uses JavaScript Proxy for method interception with full TypeScript
 * type inference — calling `client.echo(req)` has the correct types.
 */

import { RpcPattern } from './types.js';
import { RpcError, StatusCode } from './status.js';
import { getServiceInfo } from './service.js';
import type { AsterTransport, CallOptions } from './transport/base.js';
import { LocalTransport } from './transport/local.js';
import { ServiceRegistry } from './service.js';
import { ClientChain, type ClientMethod } from './client-interceptors.js';
import type { Interceptor } from './interceptors/base.js';

/**
 * Typed service client wrapper — provides service metadata accessors
 * alongside the proxy methods. Returned by createClient().
 */
export class ServiceClient<T extends object> {
  readonly proxy: AsterClient<T>;
  private readonly _name: string;
  private readonly _version: number;

  constructor(serviceClass: new (...args: any[]) => T, transport: AsterTransport, options?: ClientOptions) {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    this.proxy = createClient(serviceClass as any, transport, options) as AsterClient<T>;
    const info = getServiceInfo(serviceClass);
    this._name = info?.name ?? serviceClass.name;
    this._version = info?.version ?? 1;
  }

  /** The RPC service name. */
  get serviceName(): string { return this._name; }

  /** The RPC service version. */
  get serviceVersion(): number { return this._version; }
}

/** Options for creating a client. */
export interface ClientOptions {
  /** Default timeout in seconds for all calls (fractions allowed). A call's own `deadlineSecs` wins. */
  timeout?: number;
  /** Default metadata for all calls. */
  metadata?: Record<string, string>;
  /**
   * Client interceptors: hooks on every call pattern, plus
   * `CircuitBreakerInterceptor` / `RetryInterceptor` policies on unary and
   * client-streaming calls (see `client-interceptors.ts`).
   */
  interceptors?: Interceptor[];
}

/**
 * Type helper: extracts the client interface from a service class.
 *
 * For each @Rpc method `foo(req: A): Promise<B>`, the client has
 * `foo(req: A, opts?: CallOptions): Promise<B>`. A bidi method takes only
 * the options: `chat(opts?)`.
 */
export type AsterClient<T> = {
  [K in keyof T as T[K] extends (...args: any[]) => any ? K : never]:
    T[K] extends (...args: infer A) => infer R
      ? (...args: [...A, opts?: CallOptions]) => R
      : never;
} & {
  /** Close the underlying transport. */
  close(): Promise<void>;
  /**
   * A copy of this client whose calls (every method, every pattern) carry
   * `options`: deadline, metadata, session id, serialization mode, signal.
   * Replaces options set by an earlier `withOptions`; a call's own options
   * merge over these (metadata key by key). Cheap, so build one per request.
   */
  withOptions(options: CallOptions): AsterClient<T>;
};

/**
 * Per-call options over client-wide ones: `call` fields win, metadata merges
 * key by key (`call`'s values win). Used by every client kind (typed proxy,
 * generated clients, `ProxyClient`).
 */
export function mergeCallOptions(base?: CallOptions, call?: CallOptions): CallOptions {
  if (!call) return { ...base, metadata: base?.metadata ? { ...base.metadata } : undefined };
  const out: CallOptions = { ...base };
  for (const [k, v] of Object.entries(call)) {
    if (v !== undefined) (out as Record<string, unknown>)[k] = v;
  }
  out.metadata = base?.metadata || call.metadata ? { ...base?.metadata, ...call.metadata } : undefined;
  return out;
}

/**
 * The options argument of a typed or proxy client call, if any: `args[optsIndex]`
 * when it is a plain options object (not a request stream, not a class
 * instance such as a request message).
 */
export function callOptionsArg(args: readonly unknown[], optsIndex: number): CallOptions | undefined {
  const v = args[optsIndex];
  if (v == null || typeof v !== 'object' || Array.isArray(v)) return undefined;
  if (Symbol.asyncIterator in v || Symbol.iterator in v) return undefined;
  const proto = Object.getPrototypeOf(v);
  if (proto !== Object.prototype && proto !== null) return undefined;
  return v as CallOptions;
}

/**
 * Create a typed client from a @Service-decorated class.
 *
 * @example
 * ```ts
 * const client = createClient(EchoService, transport);
 * const result = await client.echo(new EchoRequest({ message: "hi" }));
 * ```
 */
/**
 * Create a typed client backed by an in-process LocalTransport (for testing).
 */
export function createLocalClient<T extends new (...args: any[]) => any>(
  serviceClass: T,
  registry: ServiceRegistry,
  options?: ClientOptions,
): AsterClient<InstanceType<T>> {
  const transport = new LocalTransport(registry);
  return createClient(serviceClass, transport, options);
}

/**
 * Sleep for a given number of seconds (convenience for timeout tests).
 */
export async function timeSleep(seconds: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, seconds * 1000));
}

/**
 * Build a CallOptions object with explicit timeout and metadata.
 */
export function timeouts(timeoutS: number, metadata?: Record<string, string>): CallOptions {
  return {
    deadlineSecs: timeoutS,
    metadata,
  };
}

export function createClient<T extends new (...args: any[]) => any>(
  serviceClass: T,
  transport: AsterTransport,
  options?: ClientOptions,
): AsterClient<InstanceType<T>> {
  const serviceInfo = getServiceInfo(serviceClass);
  if (!serviceInfo) {
    throw new TypeError(
      `${serviceClass.name} is not decorated with @Service.`,
    );
  }

  const chain = new ClientChain(options?.interceptors ?? []);
  return typedProxy(serviceInfo, transport, chain, options, undefined);
}

function typedProxy<S>(
  serviceInfo: NonNullable<ReturnType<typeof getServiceInfo>>,
  transport: AsterTransport,
  chain: ClientChain,
  options: ClientOptions | undefined,
  defaults: CallOptions | undefined,
): AsterClient<S> {
  return new Proxy({} as AsterClient<S>, {
    get(_target, prop: string | symbol) {
      if (prop === 'close') {
        return () => transport.close();
      }
      // Promise resolution sniffs `.then`; this is not a thenable.
      if (prop === 'then') return undefined;
      if (prop === 'withOptions' && !serviceInfo.methods.has('withOptions')) {
        return (opts: CallOptions) => typedProxy<S>(serviceInfo, transport, chain, options, opts);
      }

      if (typeof prop !== 'string') return undefined;

      const methodInfo = serviceInfo.methods.get(prop);
      if (!methodInfo) return undefined;

      return (...args: unknown[]) => {
        // A bidi call takes no request: its options come first. Every other
        // pattern takes the request (or request stream), then the options.
        const request = args[0];
        const callOpts = methodInfo.pattern === RpcPattern.BIDI_STREAM
          ? callOptionsArg(args, 0) ?? callOptionsArg(args, 1)
          : callOptionsArg(args, 1);

        const clientWide: CallOptions = {
          version: serviceInfo.version,
          metadata: options?.metadata,
          ...(options?.timeout ? { deadlineSecs: options.timeout } : {}),
        };
        const opts = mergeCallOptions(mergeCallOptions(clientWide, defaults), callOpts);
        opts.metadata ??= {};
        opts.hintType = methodInfo.requestType;

        if (!chain.empty) {
          const m: ClientMethod = {
            service: serviceInfo.name,
            method: methodInfo.name,
            pattern: methodInfo.pattern,
            idempotent: methodInfo.idempotent,
          };
          switch (methodInfo.pattern) {
            case RpcPattern.UNARY:
              return chain.unary(transport, m, request, opts);
            case RpcPattern.SERVER_STREAM:
              return chain.serverStream(transport, m, request, opts);
            case RpcPattern.CLIENT_STREAM:
              return chain.clientStream(transport, m, request as AsyncIterable<unknown>, opts);
            case RpcPattern.BIDI_STREAM:
              return chain.bidi(transport, m, opts);
          }
        }

        switch (methodInfo.pattern) {
          case RpcPattern.UNARY:
            return transport.unary(serviceInfo.name, methodInfo.name, request, opts);

          case RpcPattern.SERVER_STREAM:
            return transport.serverStream(serviceInfo.name, methodInfo.name, request, opts);

          case RpcPattern.CLIENT_STREAM:
            return transport.clientStream(
              serviceInfo.name,
              methodInfo.name,
              request as AsyncIterable<unknown>,
              opts,
            );

          case RpcPattern.BIDI_STREAM:
            return transport.bidiStream(serviceInfo.name, methodInfo.name, opts);

          default:
            throw new RpcError(
              StatusCode.UNIMPLEMENTED,
              `Unknown pattern: ${methodInfo.pattern}`,
            );
        }
      };
    },
  });
}
