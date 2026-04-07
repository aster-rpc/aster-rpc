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
  /** Default timeout in seconds for all calls. */
  timeout?: number;
  /** Default metadata for all calls. */
  metadata?: Record<string, string>;
}

/**
 * Type helper: extracts the client interface from a service class.
 *
 * For each @Rpc method `foo(req: A): Promise<B>`, the client has
 * `foo(req: A, opts?: CallOptions): Promise<B>`.
 */
export type AsterClient<T> = {
  [K in keyof T as T[K] extends (...args: any[]) => any ? K : never]:
    T[K] extends (...args: infer A) => infer R
      ? (...args: [...A, opts?: CallOptions]) => R
      : never;
} & {
  /** Close the underlying transport. */
  close(): Promise<void>;
};

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
    deadlineEpochMs: Date.now() + timeoutS * 1000,
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

  return new Proxy({} as AsterClient<InstanceType<T>>, {
    get(_target, prop: string | symbol) {
      if (prop === 'close') {
        return () => transport.close();
      }

      if (typeof prop !== 'string') return undefined;

      const methodInfo = serviceInfo.methods.get(prop);
      if (!methodInfo) return undefined;

      return (...args: unknown[]) => {
        // Last arg might be CallOptions
        let request = args[0];
        let callOpts: CallOptions | undefined;
        if (args.length > 1 && typeof args[args.length - 1] === 'object') {
          callOpts = args[args.length - 1] as CallOptions;
        }

        const opts: CallOptions = {
          ...callOpts,
          metadata: { ...options?.metadata, ...callOpts?.metadata },
        };

        if (options?.timeout && !opts.deadlineEpochMs) {
          opts.deadlineEpochMs = Date.now() + options.timeout * 1000;
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
