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
 * // Dynamic proxy: synthesises types from the producer's manifest
 * // and speaks Fory XLANG — no local type definitions needed
 * const greeter = await client.proxy("Greeter");
 * const reply = await greeter.greet({ name: "World" });
 * console.log(reply.message);
 *
 * await client.close();
 * ```
 *
 * @packageDocumentation
 */

/**
 * Declarative RPC server.
 * @group Server and Client
 */
export { AsterServer } from './runtime.js';

/**
 * Options for creating an {@link AsterServer}.
 * @group Server and Client
 */
export type { AsterServerOptions } from './runtime.js';

/**
 * High-level RPC client wrapper with admission, proxy, and typed clients.
 * @group Server and Client
 */
export { AsterClientWrapper } from './runtime.js';

/**
 * Options for creating an {@link AsterClientWrapper}.
 * @group Server and Client
 */
export type { AsterClientOptions } from './runtime.js';

/**
 * Dynamic JSON proxy client -- call any service without local types.
 * @group Server and Client
 */
export { ProxyClient } from './runtime.js';

/**
 * Thrown when a connection is rejected by the producer's admission gate.
 * @group Server and Client
 */
export { AdmissionDeniedError } from './runtime.js';

/**
 * Unified server/client configuration.
 * @group Server and Client
 */
export type { AsterConfig } from './config.js';

/**
 * Mark a class as an Aster RPC service.
 * @group Decorators
 */
export { Service, type ServiceOptions } from './decorators.js';

/**
 * Mark a method as a unary RPC.
 * @group Decorators
 */
export { Rpc } from './decorators.js';

/**
 * Mark a method as a server-streaming RPC (returns AsyncGenerator).
 * @group Decorators
 */
export { ServerStream } from './decorators.js';

/**
 * Mark a method as a client-streaming RPC.
 * @group Decorators
 */
export { ClientStream } from './decorators.js';

/**
 * Mark a method as a bidirectional-streaming RPC.
 * @group Decorators
 */
export { BidiStream } from './decorators.js';

/**
 * Register a class for cross-language Fory serialization.
 * @group Decorators
 */
export { WireType, type WireTypeOptions } from './decorators.js';

/**
 * Compose capability requirements with OR semantics.
 * @group Authorization
 */
export { anyOf } from './capabilities.js';

/**
 * Compose capability requirements with AND semantics.
 * @group Authorization
 */
export { allOf } from './capabilities.js';

/**
 * Select the wire format: XLANG (default), NATIVE, ROW, or JSON.
 * @group Serialization
 */
export { SerializationMode } from './types.js';

/**
 * Exponential-backoff parameters for retry policies.
 * @group Serialization
 */
export type { ExponentialBackoff } from './types.js';

/**
 * Retry policy declared on a service or method.
 * @group Serialization
 */
export type { RetryPolicy } from './types.js';

/**
 * Base class for all RPC failures. Carries a gRPC-compatible StatusCode.
 * @group Errors
 */
export { RpcError } from './status.js';

/**
 * gRPC-compatible status code enum (0--16).
 * @group Errors
 */
export { StatusCode, statusName } from './status.js';

/**
 * Thrown when a call violates the service contract (unknown method, bad schema).
 * @group Errors
 */
export { ContractViolationError } from './status.js';

/**
 * Typed exception subclasses, one per gRPC status code.
 * @group Errors
 */
export {
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

/**
 * Per-call context passed to every interceptor.
 * @group Interceptors
 */
export { CallContext } from './interceptors/base.js';

/**
 * Interceptor interface -- middleware that wraps every RPC call.
 * @group Interceptors
 */
export { type Interceptor } from './interceptors/base.js';

/**
 * Enforce and propagate call deadlines.
 * @group Interceptors
 */
export { DeadlineInterceptor } from './interceptors/deadline.js';

/**
 * Token-based authentication.
 * @group Interceptors
 */
export { AuthInterceptor } from './interceptors/auth.js';

/**
 * Automatic retry for idempotent methods.
 * @group Interceptors
 */
export { RetryInterceptor } from './interceptors/retry.js';

/**
 * Circuit breaker for failing endpoints.
 * @group Interceptors
 */
export { CircuitBreakerInterceptor, type CircuitBreakerOptions } from './interceptors/circuit-breaker.js';

/**
 * Structured audit logging for every RPC call.
 * @group Interceptors
 */
export { AuditLogInterceptor, type AuditEntry, type AuditLogFn } from './interceptors/audit.js';

/**
 * Collect call latency and error metrics.
 * @group Interceptors
 */
export { MetricsInterceptor } from './interceptors/metrics.js';

/**
 * Token-bucket rate limiting per service, method, or peer.
 * @group Interceptors
 */
export { RateLimitInterceptor, type RateLimitOptions } from './interceptors/rate-limit.js';

/**
 * Automatic payload compression (gzip/zstd).
 * @group Interceptors
 */
export { CompressionInterceptor } from './interceptors/compression.js';

/**
 * Enforce \`requires\` capability checks declared on services.
 * @group Interceptors
 */
export { CapabilityInterceptor } from './interceptors/capability.js';
