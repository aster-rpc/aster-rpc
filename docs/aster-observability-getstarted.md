# Aster Observability: Get Started

Logs, metrics, traces and health for Aster services, in Rust, Python,
TypeScript, Java, .NET and Go. Everything below uses synthetic names: a
service called `orders`, `example.com` collectors and `127.0.0.1` listeners.

## 1. Overview

There is one observability pipeline, and it lives in Rust (the `aster-otel`
crate). It owns the OpenTelemetry SDK, the exporters, the Prometheus renderer
and the bridge that delivers Rust log events to your language's own logger.
The language bindings are thin wrappers: they start the pipeline, read from
it, and add idiomatic helpers (a `logging` handler, an `ILogger` bridge, an
SLF4J bridge, an `slog` handler). They never parse `OTEL_*` variables
themselves, so every language behaves the same for the same environment.

Configuration is resolved once, in Rust. Highest precedence first:

1. explicit arguments to the language's `init` call;
2. `OTEL_*` and `ASTER_*` environment variables;
3. the `[observability]` table of the `AsterConfig` TOML file;
4. defaults.

**Exporters start only when `OTEL_*` asks.** A server or client that starts
without a prior `init` opens the log bridge and nothing else: no network
export happens until you set an exporter, an endpoint or call `init` with a
signal switched on. Metrics push over OTLP only when an endpoint is set; logs
are not exported over OTLP unless you ask. OTLP uses `http/protobuf`.

| Variable | Meaning | Default |
|---|---|---|
| `OTEL_SDK_DISABLED` | kill switch for all telemetry export | `false` |
| `OTEL_SERVICE_NAME` | `service.name` resource attribute | `unknown_service:<binary>` |
| `OTEL_RESOURCE_ATTRIBUTES` | extra resource attributes, `k=v,k=v` | none |
| `OTEL_TRACES_EXPORTER` | `otlp` or `none` | env-driven (see above) |
| `OTEL_METRICS_EXPORTER` | `otlp`, `prometheus`, `none` (comma list) | `otlp` only if an endpoint is set |
| `OTEL_LOGS_EXPORTER` | `otlp` or `none` | `none` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | collector base URL, e.g. `https://otel.example.com:4318` | `http://localhost:4318` |
| `OTEL_EXPORTER_OTLP_HEADERS` | request headers (often a bearer token; never logged) | none |
| `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_ENDPOINT` | per-signal override (the `_PROTOCOL`, `_TIMEOUT`, `_HEADERS` variants exist too) | shared value |
| `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | sampler, e.g. `traceidratio` and `0.25` | `parentbased_always_on` |
| `OTEL_PROPAGATORS` | `tracecontext`, `baggage`, `none` | `tracecontext,baggage` |
| `OTEL_METRIC_EXPORT_INTERVAL` | push interval, milliseconds | `60000` |
| `ASTER_LOG` | Rust log filter directives, e.g. `warn,aster=debug` | `warn,aster=info` |
| `ASTER_LOG_LEVEL` | shorthand for the `aster` targets | unset |
| `ASTER_LOG_QUEUE` | records buffered for the language logger | `8192` |
| `ASTER_LOG_STDERR` | also write Rust events to stderr (`1`, `true`) | off |
| `ASTER_HEALTH_PORT`, `ASTER_HEALTH_HOST`, `ASTER_HEALTH_ENABLED` | health listener (section 4) | off, `127.0.0.1` |

An OTLP endpoint that is plain `http://` to a non-loopback host logs a
warning; use `https://` outside localhost. Keep tokens out of command lines:
set `OTEL_EXPORTER_OTLP_HEADERS` from a secret store, not a flag.

```bash
export OTEL_SERVICE_NAME=orders
export OTEL_EXPORTER_OTLP_ENDPOINT=https://otel.example.com:4318
export OTEL_METRICS_EXPORTER=otlp
export ASTER_LOG=warn,aster=info
```

## 2. Metrics

The built-in metrics are recorded in Rust wherever calls happen, so every
language gets them with no wiring and no interceptor. There is no
`MetricsInterceptor` in any binding; add your own instruments (section 5) for
application numbers.

| Instrument | Kind | Attributes |
|---|---|---|
| `aster.rpc.server.requests_started` | counter | `rpc.service`, `rpc.method`, `aster.pattern` |
| `aster.rpc.server.requests_completed` | counter | service, method, `aster.status` |
| `aster.rpc.server.duration` | histogram, seconds | service, method, status |
| `aster.rpc.server.in_flight` | up-down counter | service |
| `aster.rpc.client.requests_completed`, `aster.rpc.client.duration` | counter, histogram | service, method, status |
| `aster.rpc.streams.active` | up-down counter | none |
| `aster.server.connections.active`, `aster.server.connections.opened` | up-down counter, counter | none |
| `aster.admission.decisions`, `aster.admission.duration` | counter, histogram (ms) | `aster.role`, `aster.result` |
| `aster.transport.bytes` | observable counter | `aster.path` (`ipv4`, `ipv6`, `relay`), `aster.direction` |
| `aster.transport.connections` | observable gauge | none |
| `aster.upcall.issued`, `aster.upcall.timeouts`, `aster.upcall.fallbacks` | counters | `aster.upcall.kind` |
| `aster.log.records_dropped`, `aster.log.audit_dropped` | counters | none |
| `aster.health.ready`, `aster.health.healthy`, `process.uptime` | observable gauges | none |

A call the router could not find is labelled `rpc.service` / `rpc.method` =
`_OTHER`, so caller-chosen names cannot mint new series.

**Prometheus.** The health server (section 4) serves `GET /metrics` as
Prometheus text (`text/plain; version=0.0.4`) from the same pipeline, with the
built-ins and your custom instruments. Names follow the Prometheus exporter:
dots become underscores, counters gain `_total`, unit `s` gains `_seconds`
(`aster_rpc_server_requests_completed_total`,
`aster_rpc_server_duration_seconds_bucket`, `process_uptime_seconds`), and
every series carries `otel_scope_name="aster"`. OpenMetrics negotiation is not
supported; the `Accept` header is ignored.

Limits you will meet: `auto_attrs` (merging call attributes into your
instruments) and a per-instrument `cardinality_limit` are accepted by the
bindings but have no effect yet; the SDK default of 2000 series per
instrument applies.

## 3. Traces

Trace context travels in call metadata as the W3C keys `traceparent`,
`tracestate` and `baggage`. Inside a handler the current trace is the call's
server span; outbound Aster calls made from a handler carry it onward
(`traceparent` and `baggage`), so a request that fans out across services and
languages stays in one trace.

- **Reserved keys.** The three keys are hidden from the metadata your handler
  sees. Read them through each language's trace API (section 5).
- **An inbound trace continues even with nothing recording.** With no
  exporter configured, with traces switched off, or with the `rpc` span
  filtered out, a valid inbound `traceparent` still becomes the call's trace
  context: the same trace id, the caller's span id and flags. A handler reads
  it and its own outbound calls pass it on, like an OpenTelemetry
  non-recording span. A malformed `traceparent` is ignored (a new root).
- **Limits.** `baggage` is dropped above 8192 bytes or 64 members.
- **Other transports.** HTTP requests honour `traceparent`, `tracestate` and
  `baggage` request headers; metadata written by an Aster client wins when
  both are present.
- **Continuing a trace from outside Aster** (a queue message, an HTTP header):
  every language has a "call under this trace context" helper, shown below.

## 4. Health

Enable the listener with `ASTER_HEALTH_PORT` (or the `[health]` TOML table
with `port`, `host`, `enabled`, or your language's config field or server
option), or start one yourself against a node. Every binding's `AsterConfig`
carries the same three settings (`health_port` = 0, `health_host` =
`127.0.0.1`, `health_enabled` = unset): the listener runs when `enabled` is
true, or when it is unset and the port is not 0. An explicit server option
wins over the config.

| Route | Meaning |
|---|---|
| `GET /healthz` | liveness: 200 `{"status":"ok","uptime_s":...}` or 503 |
| `GET /readyz` | readiness: 200 or 503 with `{"status","services","checks":{name:bool}}`; ready means the server is up **and** every named check is true |
| `GET /metrics` | Prometheus text (section 2) |

- It binds `127.0.0.1` by default. For pod probes from outside the host, set
  the host to `0.0.0.0` explicitly, and put the port behind your network
  policy: `/metrics` is not authenticated.
- A server marks itself ready after it starts serving and not ready first when
  it shuts down. Named readiness checks are **created failing**: a declared
  dependency is not ready until you set it true. Liveness ignores checks.
- Starting the listener joins the observability pipeline (or starts an
  environment-only one) and adds the Prometheus reader whatever
  `OTEL_METRICS_EXPORTER` says, so `/metrics` is never empty.
- With `OTEL_SDK_DISABLED=true` the listener still starts, and `/metrics`
  answers 503 with a one-line comment.
- Once the log bridge has been opened, a built-in `audit` check also fails
  readiness for 30 seconds after an audit log record was dropped.

## 5. Per language

Each subsection shows: init, logs, a custom counter and histogram, a custom
span, reading the current trace id and calling under a given trace context,
and health with a readiness check. Gauges and up-down counters sit next to
counters in every language. Initialise **before** you start the first server
or client: a later `init` with different settings is refused.

### 5.1 Rust

The Rust crates are `aster` (features `otel` and `health`) and `aster-otel`.
Nothing starts a pipeline implicitly except the health listener, so call
`aster_otel::init` yourself.

```rust
use aster_otel::{ObservabilityConfig, opentelemetry::{global, KeyValue}};

// Keep the handle for the life of the process; dropping the last one shuts down.
let _obs = aster_otel::init(ObservabilityConfig {
    service_name: Some("orders".into()),
    ..Default::default()           // from_env: true, so OTEL_* / ASTER_* still apply
})?;

// Custom metrics: create them after init (an instrument made before it stays a no-op).
let meter = global::meter("orders");
let placed = meter.u64_counter("orders.placed").with_description("Orders placed").build();
let latency = meter.f64_histogram("orders.db.duration").with_unit("s").build();
placed.add(1, &[KeyValue::new("region", "eu")]);
latency.record(0.012, &[KeyValue::new("op", "insert")]);
```

- **Logs.** Aster logs through `tracing`. Opt in to stderr with
  `ASTER_LOG_STDERR=1`, or compose Aster's layers into your own subscriber:
  `tracing_subscriber::registry().with(aster_otel::layers(&cfg)?).with(fmt::layer()).init()`.
  If a foreign subscriber is already installed, `init` still installs the
  providers and the handle reports it.
- **Defaults.** Unlike the bindings, a plain `init` in Rust exports traces
  over OTLP to `http://localhost:4318` unless `OTEL_TRACES_EXPORTER=none` (or
  `traces_exporter: Some(vec![])`) says otherwise.
- **Spans.** Use `tracing` spans; they are exported through OpenTelemetry. The
  span filter defaults to the log filter (`warn,aster=info`), so a span in
  your own crate needs a directive such as `ASTER_LOG=warn,aster=info,orders=info`
  (or `ObservabilityConfig::span_filter`).

  ```rust
  let span = tracing::info_span!("db.query", db.system = "postgresql");
  let _guard = span.enter();
  ```
- **Trace context.** With feature `otel`, `aster::rpc::current_trace_context()`
  returns `Option<aster_otel::TraceContext>` (trace id, span id, flags): the
  current span's, else the inbound pass-through context of the call a
  Rust-native handler is serving. `aster::rpc::with_trace_context(ctx, fut)`
  runs a future so that its client calls carry `ctx`:

  ```rust
  use aster_otel::TraceContext;
  let ctx = TraceContext::parse_traceparent(b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
      .expect("valid traceparent");
  let reply = aster::rpc::with_trace_context(ctx, client.get_order(req)).await?;
  // inside a handler: the caller's trace id as hex
  let trace_id = aster::rpc::current_trace_context()
      .map(|c| c.trace_id.iter().map(|b| format!("{b:02x}")).collect::<String>());
  ```
- **Health.** `AsterServer` starts the listener from `AsterConfig::builder().health_port(8080)`
  (or `ASTER_HEALTH_PORT`). Standalone, with feature `health`:

  ```rust
  use aster::ops::{HealthConfig, HealthServer};
  let health = HealthServer::start(&node, HealthConfig::new("127.0.0.1", 8080)).await?;
  let db = health.check("db");   // created failing
  db.set(true);
  health.set_ready(true);
  ```

  `aster::ops::route` serves the same routes from another HTTP stack and
  `aster::ops::metrics_text()` returns the scrape body.

### 5.2 Python

```python
from aster import metrics, observability, tracing, HealthServer

observability.init(service_name="orders")      # optional; before the first server
orders = metrics.counter("orders.placed", "Orders placed")
latency = metrics.histogram("orders.db.duration", "DB time", "s")
orders.add(1, {"region": "eu"})
latency.record(0.012, {"op": "insert"})

with tracing.span("db.query", {"db.system": "postgresql"}):   # also `async with`
    ...

@tracing.traced("orders.place")                # decorator form, sync or async
async def place(order): ...
```

- **Logs** arrive in the standard `logging` module on loggers named after the
  Rust target (`aster::rpc::dispatch` becomes `aster.rpc.dispatch`), with
  `trace_id`, `span_id`, `rpc_service` and `rpc_method` attributes. Rust
  filters by the `aster` logger's level: call `aster.logging.sync_level()`
  after `dictConfig` / `basicConfig`. `aster.logging.LoggingFilter`,
  `structlog_processor` and `loguru_patcher` add the current trace ids to your
  own records.
- **Trace context.** `tracing.current_trace_id()` (also `current_span_id()`,
  `current_traceparent()`, `current_baggage()`); to continue a trace from
  elsewhere:

  ```python
  with tracing.with_trace_context("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"):
      reply = await client.get_order(req)      # carries that trace
  ```
- **Health.** `AsterConfig(health_port=8080)` makes `AsterServer` serve the
  routes (`server.health`). Standalone:

  ```python
  health = await HealthServer.start(node, "127.0.0.1", 8080)
  db = health.check("db")        # created failing
  db.set(True)
  ```
- **Fork caveat.** Initialise in each worker, not in a pre-fork master. A child
  forked after `init` has its telemetry disabled with one warning; a child
  forked before `init` works.

### 5.3 TypeScript

```ts
import { initObservability, metrics, tracing, setLogSink, pinoSink, shutdownObservability } from '@aster-rpc/aster';
import pino from 'pino';

await initObservability({ serviceName: 'orders' });   // optional; before the first server
setLogSink(pinoSink(pino()));                         // Rust logs into your logger

const placed = metrics.counter('orders.placed', { description: 'Orders placed' });
const latency = metrics.histogram('orders.db.duration', { unit: 's' });
placed.add(1, { region: 'eu' });
latency.record(0.012, { op: 'insert' });

await tracing.withSpanAsync('db.query', { 'db.system': 'postgresql' }, async (span) => {
  span.addEvent('connected');
  /* ... */
});

// on exit
await shutdownObservability();
```

- **Logs.** Rust records go to a sink: `setLogSink(sink)` with `pinoSink`,
  `winstonSink` or your own `(record) => void` (the default `consoleSink`
  prints warnings and errors). `setLogFilter('warn,aster=debug')` changes the
  Rust filter at runtime. `tracingMixin` (pino) and `winstonTracingFormat`
  add the current trace ids to your own lines.
- **Trace context.** `currentTraceId()` (also `currentSpanId()`,
  `currentBaggage()`, `currentTraceparent()`); `withTraceContext(traceparent, fn)`
  runs `fn` so that calls inside it carry that trace:

  ```ts
  import { currentTraceId, withTraceContext } from '@aster-rpc/aster';
  const reply = await withTraceContext(
    '00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01',
    () => client.getOrder(req),
  );
  ```
- **Health.** `new AsterServer({ ..., config: { healthPort: 8080 } })` serves
  the routes (`server.health`). Standalone, from a node:

  ```ts
  const health = await node.startHealth({ host: '127.0.0.1', port: 8080 });
  health.check('db').set(true);   // created failing
  ```

- **Your own logs over OTLP (path B, opt-in).** `emitLog('info', 'charged', { order })`
  or `new AsterLogger({ otlp: true, name: 'orders' })` writes your records into the
  Rust OTel logs SDK with the current trace ids. It exports only when logs export is on
  (`exportOtlpLogs: true` or `OTEL_LOGS_EXPORTER`), is a quiet no-op otherwise, and cannot
  loop back into the Rust log bridge.

### 5.4 Java

Packages `site.aster.otel` and `site.aster.health`. Java uses SLF4J 2.x.

```java
import site.aster.otel.*;

Observability.init(ObservabilityConfig.builder().serviceName("orders").build());

Metrics.Counter placed = Metrics.counter("orders.placed", "Orders placed", "1");
Metrics.Histogram latency = Metrics.histogram("orders.db.duration", "DB time", "s");
placed.add(1, Map.of("region", "eu"));
latency.record(0.012, Map.of("op", "insert"));

Tracing.withSpan("db.query", Map.of("db.system", "postgresql"), () -> {
  /* ... */
  return null;
});
```

- **Logs** go to SLF4J on loggers named after the Rust target (dots, so
  `aster.rpc.dispatch`), with `trace_id` and `span_id` as structured
  key/values. `Tracing.withSpan` and the dispatcher also set the SLF4J MDC for
  the block, so a pattern with `%X{trace_id}` correlates your own records.
  Rust follows your logger's level; `Observability.syncLevel()` re-reads it.
- **Trace context.** `Tracing.currentTraceId()` (also `currentSpanId()`,
  `currentTraceparent()`, `currentBaggage()`), carried on a `ScopedValue`.
  A plain thread pool does not inherit it: wrap with `Tracing.bind(runnable)`
  or `Tracing.propagating(executor)`.

  ```java
  Order r = Tracing.withTraceContext(
      "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
      Map.of(),
      () -> client.call(addr, "orders", "getOrder", req, Order.class).get(15, TimeUnit.SECONDS));
  ```
- **Health.** `AsterConfig` carries `healthPort` / `healthHost` / `healthEnabled`
  (`ASTER_HEALTH_*`, `[health]`), and `AsterServer` starts the listener from
  them. A builder `.health(...)` call overrides the config.

  ```java
  AsterServer server = AsterServer.builder()
      .config(AsterConfig.builder().healthPort(8080).build())   // or ASTER_HEALTH_PORT=8080
      /* ... services ... */
      // or explicitly: .health("127.0.0.1", 8080)
      .build().get();             // ready after start; closed with the server
  server.health().check("db").set(true);
  ```

  Standalone: `HealthServer.start(node, "127.0.0.1", 8080)`.

- **Your own logs over OTLP (path B, opt-in).** `LogEmit.emit(Level.INFO, "orders", "charged", Map.of("order", id))`
  writes a record into the Rust OTel logs SDK with the current trace context;
  `LogEmit.emit(LoggingEvent)` is the hook for a logging backend's appender (it skips records the
  Rust bridge delivered, `aster_origin=rust`). Needs `otlpLogs(true)` or `OTEL_LOGS_EXPORTER`;
  a quiet no-op otherwise.

### 5.5 .NET

Namespaces `Aster.Observability` and `Aster.Health`. Logs use
`Microsoft.Extensions.Logging`; traces use `System.Diagnostics.Activity`.

```csharp
using Aster.Observability;
using Aster.Health;

AsterObservability.Init(new ObservabilityOptions { ServiceName = "orders" });
AsterLogging.UseLoggerFactory(loggerFactory);          // Rust logs into ILogger

var placed = Metrics.Counter("orders.placed", "Orders placed");
var latency = Metrics.Histogram("orders.db.duration", "DB time", "s");
placed.Add(1, ("region", "eu"));
latency.Record(0.012, ("op", "insert"));

using (var span = Tracing.StartSpan("db.query"))        // an Activity of source "Aster"
{
    span?.SetTag("db.system", "postgresql");
}
```

- **Logs** go to the `ILoggerFactory` you pass, with categories named after the
  Rust target and `TraceId` / `SpanId` in the scope. `AsterLogging.SyncLevel()`
  makes Rust filter by the factory's level for the `aster` category.
- **Trace context.** Aster traces are `Activity` objects. `Tracing.CurrentTraceId`
  (also `CurrentSpanId`, `CurrentTraceparent`, `CurrentBaggage`);
  `Tracing.WithTraceContext(traceparent)` returns a scope:

  ```csharp
  using (Tracing.WithTraceContext("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"))
  {
      var reply = await conn.UnaryAsync<GetOrder, Order>("orders", "getOrder", req);
  }
  ```

  Do not also subscribe the OpenTelemetry .NET SDK to the `Aster` activity
  source: Aster exports those spans itself, and doing both double-exports.
- **Health.** `AsterConfig` has `HealthPort` / `HealthHost` / `HealthEnabled`
  (`ASTER_HEALTH_*`, `[health]`) and `AsterServer` starts the listener from them;
  `AsterServerOptions.Health` overrides the config:

  ```csharp
  var server = await AsterServer.StartAsync(new AsterServerOptions { /* ... */ Config = AsterConfig.FromEnv() with { HealthPort = 8080 } });
  // or explicitly: Health = new HealthOptions { Port = 8080 }
  server.Health?.Check("db").Set(true);
  // standalone:
  using var health = HealthServer.Start(node, new HealthOptions { Port = 8080 });
  ```

- **Your own logs over OTLP (path B, opt-in).** `builder.Logging.AddAsterLogEmit()` forwards your
  `ILogger` records to the Rust OTel logs SDK with the current `Activity` trace ids
  (`AsterLogEmit.Emit(...)` is the direct call). Records marked `aster.origin=rust` are skipped.
  Needs `ObservabilityOptions.OtlpLogs = true` or `OTEL_LOGS_EXPORTER`; a quiet no-op otherwise.

### 5.6 Go

Package `aster` (`github.com/aster-rpc/aster-go`). Logs use `log/slog`.

```go
slog.SetDefault(slog.New(aster.NewTraceHandler(slog.NewJSONHandler(os.Stderr, nil))))
aster.SetLogger(slog.Default())                      // Rust logs into slog

if err := aster.InitObservability(aster.ObservabilityConfig{ServiceName: "orders"}); err != nil { /* ... */ }
defer aster.ShutdownObservability(5 * time.Second)

placed, _ := aster.NewCounter("orders.placed", "Orders placed", "1")
latency, _ := aster.NewHistogram("orders.db.duration", "DB time", "s")
placed.Add(ctx, 1, aster.StringAttr("region", "eu"))
latency.Record(ctx, 0.012, aster.StringAttr("op", "insert"))

err := aster.WithSpan(ctx, "db.query", func(ctx context.Context) error {
    return nil
}, aster.WithSpanAttributes(aster.StringAttr("db.system", "postgresql")))
```

- **Logs** go to the `slog.Logger` you set with `SetLogger` (else
  `slog.Default()`); Rust follows that logger's level, re-checked every five
  seconds (`aster.SyncLogLevel()` forces it). `NewTraceHandler` adds
  `trace_id` and `span_id` from the `context.Context` you log with
  (`InfoContext`).
- **Trace context.** Handler contexts carry the call's server span.
  `aster.CurrentTraceID(ctx)` returns the 32-hex id; `aster.StartSpan` returns
  a context in which Aster calls continue the span. To continue an outside trace:

  ```go
  tctx, err := aster.WithTraceContext(ctx,
      "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", nil)
  r, err := aster.Call[GetOrder, Order](tctx, conn, "orders", "getOrder", &req)
  ```
- **Health.** `AsterConfig` has `HealthPort` / `HealthHost` / `HealthEnabled`
  (`ASTER_HEALTH_*` through `ConfigFromEnv`, `[health]` through `ConfigFromTOML`) and
  `StartServer` starts the listener from them; `ServerOptions.Health` overrides:

  ```go
  cfg, _ := aster.ConfigFromEnv()   // or: cfg.HealthPort = 8080
  srv, err := aster.StartServer(ctx, aster.ServerOptions{Config: cfg /* ... */})
  // or explicitly: Health: &aster.HealthConfig{Port: 8080}
  srv.Health().SetCheck("db", true)
  // standalone: h, err := aster.StartHealth(node, &aster.HealthConfig{Host: "127.0.0.1", Port: 8080})
  ```

- **Your own logs over OTLP (path B, opt-in).** Wrap your handler:
  `slog.New(aster.NewEmitHandler(aster.NewTraceHandler(inner)))`. Records are also written
  into the Rust OTel logs SDK, with trace ids from the context (`aster.EmitLog` is the direct call).
  Records the Rust bridge delivered (`aster_origin=rust`) are not re-emitted. Needs
  `OTLPLogs: true` or `OTEL_LOGS_EXPORTER`; a quiet no-op otherwise.

## 6. Quick checks

```bash
curl -s http://127.0.0.1:8080/healthz
curl -s http://127.0.0.1:8080/readyz
curl -s http://127.0.0.1:8080/metrics | grep aster_rpc_server
```

If `/readyz` is 503, read the `checks` object: a check you declared and never
set true is the usual cause.
