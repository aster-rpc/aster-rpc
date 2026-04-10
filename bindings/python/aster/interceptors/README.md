# Aster Interceptors

Interceptors are middleware that run before and after every RPC call. They
can validate, transform, or reject requests and responses.

**Default:** `DeadlineInterceptor` is wired in by default on both `Server`
and `AsterServer`. Pass `interceptors=[...]` to override (an empty list
disables all defaults).

## Wiring interceptors

```python
from aster.interceptors.deadline import DeadlineInterceptor
from aster.interceptors.rate_limit import RateLimitInterceptor
from aster.interceptors.metrics import MetricsInterceptor

server = AsterServer(
    services=[MyService()],
    interceptors=[
        DeadlineInterceptor(),
        RateLimitInterceptor(rate=1000, per="peer"),
        MetricsInterceptor(),
    ],
)
```

## Available interceptors

### DeadlineInterceptor (default)

Rejects requests whose deadline has already expired on receipt, accounting
for clock skew. Complements the handler-level deadline enforcement
(`MAX_HANDLER_TIMEOUT_S`) which caps execution time.

```python
DeadlineInterceptor(skew_tolerance_ms=5000)
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `skew_tolerance_ms` | 5000 | Clock-skew tolerance in milliseconds |

### RateLimitInterceptor

Token-bucket rate limiter. Rejects excess requests with `RESOURCE_EXHAUSTED`.

```python
RateLimitInterceptor(rate=100.0, burst=None, per="global")
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `rate` | 100.0 | Requests per second |
| `burst` | rate | Max burst size |
| `per` | `"global"` | Granularity: `"global"`, `"service"`, `"method"`, or `"peer"` |

### CapabilityInterceptor

Gate 3 access control. Evaluates `@service(requires=...)` and
`@rpc(requires=...)` against the caller's admission attributes.
Automatically wired when any service has `requires` set.

```python
CapabilityInterceptor(service_map=registry.services)
```

### AuthInterceptor

Injects and/or validates auth tokens in request metadata.

```python
AuthInterceptor(
    token_provider="my-secret-token",   # or callable
    validator=lambda token: token == "my-secret-token",
    metadata_key="authorization",
    scheme="Bearer",
)
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `token_provider` | None | Static string or `() -> str` callable |
| `validator` | None | Static string or `(str) -> bool` callable |
| `metadata_key` | `"authorization"` | Metadata key for the token |
| `scheme` | `"Bearer"` | Prefix stripped before validation |

### MetricsInterceptor

Collects RED metrics (Rate, Errors, Duration). Optional OpenTelemetry
integration; falls back to in-memory counters when OTel is unavailable.

```python
metrics = MetricsInterceptor()
# After some calls:
print(metrics.snapshot())
# {'started': 100, 'succeeded': 95, 'failed': 5, 'in_flight': 2, ...}
```

### AuditLogInterceptor

Structured audit logging for compliance and debugging.

```python
AuditLogInterceptor(
    sink=my_list,           # optional: collects event dicts
    logger=my_logger,       # optional: defaults to module logger
)
```

### CircuitBreakerInterceptor

CLOSED -> OPEN -> HALF_OPEN state machine. Stops dispatching to handlers
that are consistently failing.

```python
CircuitBreakerInterceptor(
    failure_threshold=3,
    recovery_timeout=5.0,
    half_open_max_calls=1,
)
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `failure_threshold` | 3 | Failures before opening circuit |
| `recovery_timeout` | 5.0 | Seconds before trying half-open probe |
| `half_open_max_calls` | 1 | Max concurrent calls in half-open state |

### CompressionInterceptor

Controls per-call zstd compression policy.

```python
CompressionInterceptor(threshold=4096, enabled=True)
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `threshold` | 4096 | Min payload bytes before compressing |
| `enabled` | True | Set False to disable compression |

### RetryInterceptor

Client-side retry hints. Attaches backoff information to errors so the
client transport can decide whether to retry.

```python
RetryInterceptor(
    policy=RetryPolicy(max_attempts=3, backoff_multiplier=1.5),
    retryable_codes={StatusCode.UNAVAILABLE},
)
```

Only retries idempotent calls (`@rpc(idempotent=True)`).
