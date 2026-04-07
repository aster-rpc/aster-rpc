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
