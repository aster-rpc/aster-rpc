/**
 * LocalTransport — in-process transport for testing.
 *
 * Executes RPC calls directly against a ServiceRegistry without
 * network I/O or serialization. Useful for unit testing service logic.
 */

import { StatusCode, RpcError } from '../status.js';
import { RpcPattern } from '../types.js';
import type { ServiceRegistry, ServiceInfo, MethodInfo } from '../service.js';
import type { AsterTransport, CallOptions, BidiChannel } from './base.js';

/**
 * In-process transport that dispatches directly to registered services.
 *
 * @example
 * ```ts
 * const registry = new ServiceRegistry();
 * registry.register(new EchoService());
 * const transport = new LocalTransport(registry);
 *
 * const result = await transport.unary("Echo", "echo", { message: "hi" });
 * ```
 */
export class LocalTransport implements AsterTransport {
  private registry: ServiceRegistry;

  constructor(registry: ServiceRegistry) {
    this.registry = registry;
  }

  async unary(
    service: string,
    method: string,
    request: unknown,
    _opts?: CallOptions,
  ): Promise<unknown> {
    const [svcInfo, methodInfo] = this.resolve(service, method);
    this.assertPattern(methodInfo, RpcPattern.UNARY);
    const handler = methodInfo.handler!;
    return handler.call(svcInfo.instance, request);
  }

  async *serverStream(
    service: string,
    method: string,
    request: unknown,
    _opts?: CallOptions,
  ): AsyncIterable<unknown> {
    const [svcInfo, methodInfo] = this.resolve(service, method);
    this.assertPattern(methodInfo, RpcPattern.SERVER_STREAM);
    const handler = methodInfo.handler!;
    const gen = handler.call(svcInfo.instance, request);
    yield* gen;
  }

  async clientStream(
    service: string,
    method: string,
    requests: AsyncIterable<unknown>,
    _opts?: CallOptions,
  ): Promise<unknown> {
    const [svcInfo, methodInfo] = this.resolve(service, method);
    this.assertPattern(methodInfo, RpcPattern.CLIENT_STREAM);
    const handler = methodInfo.handler!;
    return handler.call(svcInfo.instance, requests);
  }

  bidiStream(
    service: string,
    method: string,
    _opts?: CallOptions,
  ): BidiChannel {
    const [svcInfo, methodInfo] = this.resolve(service, method);
    this.assertPattern(methodInfo, RpcPattern.BIDI_STREAM);

    // Create a simple in-process bidi channel
    const requestQueue: unknown[] = [];
    let requestResolve: (() => void) | null = null;
    let sendClosed = false;

    const handler = methodInfo.handler!;

    // Request iterable that the handler reads from
    const requestIterable: AsyncIterable<unknown> = {
      [Symbol.asyncIterator]() {
        return {
          async next() {
            while (requestQueue.length === 0 && !sendClosed) {
              await new Promise<void>((r) => { requestResolve = r; });
            }
            if (requestQueue.length > 0) {
              return { value: requestQueue.shift(), done: false };
            }
            return { value: undefined, done: true };
          },
        };
      },
    };

    // Start the handler
    const responseGen = handler.call(svcInfo.instance, requestIterable);

    const channel: BidiChannel = {
      async send(msg: unknown) {
        requestQueue.push(msg);
        requestResolve?.();
        requestResolve = null;
      },

      async *[Symbol.asyncIterator]() {
        yield* responseGen;
      },

      async close() {
        sendClosed = true;
        requestResolve?.();
        requestResolve = null;
      },

      async waitForTrailer(): Promise<[StatusCode, string]> {
        return [StatusCode.OK, ''];
      },
    };

    return channel;
  }

  async close(): Promise<void> {
    // Nothing to close for in-process transport
  }

  private resolve(service: string, method: string): [ServiceInfo, MethodInfo] {
    const result = this.registry.lookupMethod(service, method);
    if (!result) {
      throw new RpcError(
        StatusCode.NOT_FOUND,
        `${service}/${method} not found`,
      );
    }
    return result;
  }

  private assertPattern(methodInfo: MethodInfo, expected: RpcPattern): void {
    if (methodInfo.pattern !== expected) {
      throw new RpcError(
        StatusCode.UNIMPLEMENTED,
        `${methodInfo.name} is ${methodInfo.pattern}, not ${expected}`,
      );
    }
  }
}
