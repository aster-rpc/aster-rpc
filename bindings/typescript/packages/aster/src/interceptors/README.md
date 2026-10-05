# Aster Interceptors (TypeScript)

Interceptors are middleware that run before and after every RPC call. They
can validate, transform, or reject requests and responses.

**Default:** none. Deadlines are enforced by the Rust server (a call's budget is
`min(client deadline, method timeout, handler timeout)`; when it passes, the
client gets `DEADLINE_EXCEEDED` and the handler's `ctx.signal` aborts), and
`requires` is enforced by Rust Gate 3.

## Wiring interceptors

```typescript
import { RpcServer } from 'aster';
import { RateLimitInterceptor } from 'aster/interceptors/rate-limit';

const server = new RpcServer({
  registry,
  interceptors: [
    new RateLimitInterceptor({ perPeerRps: 1000 }),
  ],
});
```

## Available interceptors

### RateLimitInterceptor

Token-bucket rate limiter with independent global, per-service, per-method,
and per-peer buckets. Rejects excess requests with `RESOURCE_EXHAUSTED`.

```typescript
new RateLimitInterceptor({
  globalRps: 10000,       // default 0 (disabled)
  perServiceRps: 5000,    // default 0 (disabled)
  perMethodRps: 1000,     // default 0 (disabled)
  perPeerRps: 500,        // default 0 (disabled)
})
```

Multiple limits can be active simultaneously; a request must pass all
enabled buckets. Burst defaults to 2x the rate.

### CapabilityInterceptor

Gate 3 access control. Evaluates `@Rpc({ requires: ... })` against the
caller's admission attributes (`aster.role`). Supports `role`, `any_of`,
and `all_of` requirement kinds.

```typescript
const cap = new CapabilityInterceptor();
cap.setRequirement('MyService', 'myMethod', 'admin');
// or: cap.setRequirement('MyService', 'myMethod', { kind: 'any_of', roles: ['admin', 'ops'] });
```

### AuthInterceptor

Injects and/or validates auth tokens in request metadata.

```typescript
new AuthInterceptor(
  () => 'my-token',                    // tokenProvider (optional)
  (token) => token === 'my-token',     // tokenValidator (optional)
  'authorization',                     // headerKey (default)
)
```

### Metrics

There is no metrics interceptor: RED metrics (`aster.rpc.server.requests_started`,
`...requests_completed`, `...duration`, `...in_flight`) are recorded in Rust for
every call and served at `/metrics` / exported over OTLP. For your own
instruments use `metrics.counter(...)` and `tracing.withSpan(...)`
(`@aster-rpc/aster`).

### AuditLogInterceptor

Structured audit logging for compliance and debugging.

```typescript
new AuditLogInterceptor((entry) => myLogger.info(entry))
// Default: console.log(JSON.stringify(entry))
```

### CircuitBreakerInterceptor

CLOSED -> OPEN -> HALF_OPEN state machine. Stops dispatching to handlers
that are consistently failing.

```typescript
new CircuitBreakerInterceptor({
  failureThreshold: 5,      // default 5
  resetTimeoutMs: 30_000,   // default 30s
  halfOpenMaxCalls: 1,      // default 1
})
```

### CompressionInterceptor

Sets `accept-encoding: zstd` in request metadata. Actual compression
is handled by the transport/codec layer.

```typescript
new CompressionInterceptor(threshold?: number)  // default 4096
```

### RetryInterceptor

Client-side retry hints. Attaches `retry_after_ms` and `retry_attempt`
to error details so the client transport can decide whether to retry.

```typescript
new RetryInterceptor({
  maxAttempts: 3,
  backoffMultiplier: 1.5,
  maxBackoffMs: 30_000,
})
```

Only retries idempotent calls. Retryable codes: `UNAVAILABLE`,
`DEADLINE_EXCEEDED`, `ABORTED`, `RESOURCE_EXHAUSTED`.
