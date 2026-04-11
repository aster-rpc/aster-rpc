/**
 * @aster-rpc/aster -- Public API Reference
 *
 * Curated public surface of the Aster RPC framework, mirroring the
 * Python `aster.public` module. This is the entry point used to
 * generate the TypeScript API reference at /api/typescript/.
 *
 * **In your code, import from `@aster-rpc/aster` directly**, not from
 * `@aster-rpc/aster/public`. This file exists only so the documentation
 * pipeline has a focused, opinionated view of the user-facing surface
 * (the full `index.ts` re-exports many internal types that are not
 * intended for direct use).
 *
 * ## Getting started
 *
 * **Producer (server):**
 *
 * ```typescript
 * import { AsterServer, Service, Rpc, WireType } from '@aster-rpc/aster';
 *
 * @WireType("myapp/GreetRequest")
 * class GreetRequest {
 *   name = "";
 *   constructor(init?: Partial<GreetRequest>) { if (init) Object.assign(this, init); }
 * }
 *
 * @WireType("myapp/GreetResponse")
 * class GreetResponse {
 *   message = "";
 *   constructor(init?: Partial<GreetResponse>) { if (init) Object.assign(this, init); }
 * }
 *
 * @Service({ name: "Greeter", version: 1 })
 * class Greeter {
 *   @Rpc({ request: GreetRequest, response: GreetResponse })
 *   async greet(req: GreetRequest): Promise<GreetResponse> {
 *     return new GreetResponse({ message: `Hello, ${req.name}!` });
 *   }
 * }
 *
 * const server = new AsterServer({ services: [new Greeter()] });
 * await server.start();
 * console.log(server.address); // share this with consumers
 * await server.serve();
 * ```
 *
 * **Consumer (client):**
 *
 * ```typescript
 * import { AsterClientWrapper } from '@aster-rpc/aster';
 *
 * const client = new AsterClientWrapper({ address: "aster1..." });
 * await client.connect();
 *
 * // Dynamic proxy: speaks JSON, no local types needed
 * const greeter = client.proxy("Greeter");
 * const reply = await greeter.greet({ name: "World" });
 * console.log(reply.message);
 *
 * await client.close();
 * ```
 *
 * @packageDocumentation
 */

// Server / client
export {
  AsterServer,
  AsterClientWrapper,
  ProxyClient,
  AdmissionDeniedError,
  type AsterServerOptions,
  type AsterClientOptions,
} from './runtime.js';

export { type AsterConfig } from './config.js';

// Decorators -- define services and types
export {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  WireType,
  type ServiceOptions,
  type WireTypeOptions,
} from './decorators.js';

// Authorization -- compose capability requirements
export { anyOf, allOf } from './capabilities.js';

// Serialization -- pick wire format
export {
  SerializationMode,
  type ExponentialBackoff,
  type RetryPolicy,
} from './types.js';

// Errors -- typed exceptions for RPC failures
export {
  RpcError,
  StatusCode,
  statusName,
  ContractViolationError,
  // gRPC-mirror status code subclasses
  CancelledError,
  UnknownRpcError,
  InvalidArgumentError,
  DeadlineExceededError,
  NotFoundError,
  AlreadyExistsError,
  PermissionDeniedError,
  ResourceExhaustedError,
  FailedPreconditionError,
  AbortedError,
  OutOfRangeError,
  UnimplementedError,
  InternalError,
  UnavailableError,
  DataLossError,
  UnauthenticatedError,
} from './status.js';

// Interceptors -- middleware that wraps every RPC call
export {
  CallContext,
  type Interceptor,
} from './interceptors/base.js';
export { DeadlineInterceptor } from './interceptors/deadline.js';
export { AuthInterceptor } from './interceptors/auth.js';
export { RetryInterceptor } from './interceptors/retry.js';
export { CircuitBreakerInterceptor, type CircuitBreakerOptions } from './interceptors/circuit-breaker.js';
export { AuditLogInterceptor, type AuditEntry, type AuditLogFn } from './interceptors/audit.js';
export { MetricsInterceptor } from './interceptors/metrics.js';
export { RateLimitInterceptor, type RateLimitOptions } from './interceptors/rate-limit.js';
export { CompressionInterceptor } from './interceptors/compression.js';
export { CapabilityInterceptor } from './interceptors/capability.js';
