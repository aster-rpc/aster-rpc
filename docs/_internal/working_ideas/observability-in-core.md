# Observability in Core — OTel SDK as a First-Class Aster Concern

**Status:** Working idea
**Date:** 2026-05-05
**Companion docs:**
- identity-worked-example.md (deleted 2026-06-10, in git history; superseded by [../trust-directory.md](../trust-directory.md)) — Portal use case; the `tenant_id` propagation question motivates the multi-tenancy section here
- `bindings/python/aster/interceptors/metrics.py`, `bindings/typescript/.../metrics.ts`, `bindings/java/.../MetricsInterceptor.java` — current per-binding implementations this design supersedes

---

## Why this doc

Today every binding has its own `MetricsInterceptor` that:

1. Tries to import the language-native OpenTelemetry SDK at startup.
2. Falls back to in-memory counters when OTel is missing or unconfigured.
3. Emits RED metrics + one span per RPC.

Three problems with that:

- **Per-language SDK quality varies.** Java OTel SDK is mature; Python is fine; TypeScript is workable; the bindings end up with subtly different behaviour, attribute keys, and exporter quirks.
- **Configuration drifts.** An operator deploying a polyglot fleet (Portal control plane in Java, SDK clients in Python, browser in TS) configures OTel three different ways and hopes the data lines up downstream.
- **Trace context doesn't propagate cleanly across FFI.** A handler in Python that calls into Rust core (which then dispatches to another handler in JavaScript via WASM, etc.) breaks the OTel `Context` chain because each language SDK has its own context-storage assumption.

The fix: **one Rust-owned OTel pipeline in core; every binding ships a thin wrapper.** Day-1 parity, single configuration source, trace context survives FFI by construction.

---

## The shift in one sentence

Move the OpenTelemetry SDK + OTLP exporter into a new `aster-otel` core crate; every binding ships a small `metrics` / `tracing` module that creates instrument handles wrapping Rust-side instruments; updates cross FFI as cheap atomic operations into the same SDK that emits Aster's built-in metrics and spans.

---

## Operator config — single source of truth

The operator configures observability **once**, in code or via standard OpenTelemetry environment variables, and it applies across every binding identically:

```bash
# Standard OTel env vars — Aster honours them automatically
export OTEL_EXPORTER_OTLP_ENDPOINT=https://otel.portal.io:4317
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer ..."
export OTEL_RESOURCE_ATTRIBUTES="service.name=portal-cp,deployment.environment=prod"
export OTEL_TRACES_SAMPLER=parentbased_traceidratio
export OTEL_TRACES_SAMPLER_ARG=0.1
```

Or programmatically (any binding, identical shape):

```python
from aster import AsterServer, Observability

server = AsterServer(
    services=[PortalService()],
    observability=Observability(
        service_name="portal-cp",
        otlp_endpoint="https://otel.portal.io:4317",
        otlp_headers={"Authorization": "Bearer ..."},
        trace_sample_ratio=0.1,
        resource_attrs={"deployment.environment": "prod", "region": "us-east-1"},
    ),
)
```

No language-specific OTel SDK imports, no per-language exporter quirks. The Rust SDK does all OTLP encoding, batching, retry, and TLS.

---

## Built-in metrics — emitted automatically

Once `Observability(...)` is configured, core emits these without any binding involvement:

```
# RPC-level (replaces today's per-binding MetricsInterceptor)
aster.rpc.requests_started      counter   {service, method, pattern}
aster.rpc.requests_completed    counter   {service, method, status}
aster.rpc.duration              histogram {service, method, status}    unit=s
aster.rpc.in_flight             up_down   {service}

# Transport (iroh-level)
aster.transport.connections_active   gauge     {peer_kind}
aster.transport.connection_attempts  counter   {result}
aster.transport.bytes_sent           counter   {peer_id, direction}
aster.transport.bytes_received       counter
aster.transport.handshake_duration   histogram

# Codec / framing
aster.codec.encode_duration  histogram {codec}
aster.codec.decode_duration  histogram {codec}
aster.codec.bytes_in/out     counter   {codec, direction}

# Trust / enrollment
aster.enrollment.requests        counter   {proof_kind, result}
aster.rcan.verifications         counter   {result, reason}
```

Plus **one server span per RPC**, parented to whatever W3C trace context arrived on the wire (carried in Aster framing as a `traceparent` header equivalent).

These come for free with no FFI cost — core emits them directly into the same Rust-side meter that user-defined metrics use.

---

## Custom metrics DX — same shape across bindings

The API surface mirrors OpenTelemetry's standard meter API. A developer who has used OTel anywhere already knows it.

### Python

```python
from aster import metrics, tracing
from aster.decorators import rpc

# Declare metrics once, at module load.
WORKSPACES_CREATED = metrics.counter(
    "portal.workspace.created",
    description="Workspaces created",
    unit="1",
)
LAUNCH_DURATION = metrics.histogram(
    "portal.workspace.launch_duration",
    description="Time from request to ready",
    unit="ms",
)
ACTIVE_WORKSPACES = metrics.up_down_counter(
    "portal.workspace.active",
    description="Currently active workspaces",
    unit="1",
)

class PortalService:
    @rpc
    async def launch_workspace(self, ctx, req):
        # Span auto-parents to the RPC's server span (created by core).
        with tracing.span("workspace.launch", attrs={"recipe": req.recipe}) as span:
            start = time.perf_counter()
            workspace = await self._do_launch(req)
            elapsed_ms = (time.perf_counter() - start) * 1000

            attrs = {"recipe": req.recipe}
            WORKSPACES_CREATED.add(1, attrs)
            LAUNCH_DURATION.record(elapsed_ms, {**attrs, "boundary": workspace.boundary})
            ACTIVE_WORKSPACES.add(1, attrs)

            span.set_attribute("workspace.id", workspace.id)
            return workspace

    @rpc
    async def stop_workspace(self, ctx, req):
        await self._do_stop(req.id)
        ACTIVE_WORKSPACES.add(-1, {"recipe": req.recipe})
        return {}
```

### TypeScript

```typescript
import { metrics, tracing, rpc } from '@aster/aster';

const workspacesCreated = metrics.counter('portal.workspace.created', {
  description: 'Workspaces created',
  unit: '1',
});
const launchDuration = metrics.histogram('portal.workspace.launch_duration', {
  description: 'Time from request to ready',
  unit: 'ms',
});
const activeWorkspaces = metrics.upDownCounter('portal.workspace.active', {
  description: 'Currently active workspaces',
  unit: '1',
});

class PortalService {
  @rpc
  async launchWorkspace(ctx: CallContext, req: LaunchRequest): Promise<Workspace> {
    return tracing.withSpan('workspace.launch', { recipe: req.recipe }, async (span) => {
      const start = performance.now();
      const workspace = await this.doLaunch(req);
      const attrs = { recipe: req.recipe };

      workspacesCreated.add(1, attrs);
      launchDuration.record(performance.now() - start, { ...attrs, boundary: workspace.boundary });
      activeWorkspaces.add(1, attrs);

      span.setAttribute('workspace.id', workspace.id);
      return workspace;
    });
  }
}
```

### Java (requires Java 25+)

The Java binding is built on Project Panama (Foreign Function & Memory API) and `ScopedValue`, both stable in Java 25. Targeting 25 as the minimum lets us lean on the modern primitives directly rather than carrying compatibility shims for older runtimes.

```java
public class PortalService {
    // Intern attribute keys once, reuse forever — saves UTF-8 encoding per emit.
    private static final AttributeKey<String> RECIPE   = AttributeKey.stringKey("recipe");
    private static final AttributeKey<String> BOUNDARY = AttributeKey.stringKey("boundary");

    private static final LongCounter WORKSPACES_CREATED =
        Metrics.counter("portal.workspace.created")
               .description("Workspaces created")
               .unit("1")
               .build();

    private static final DoubleHistogram LAUNCH_DURATION =
        Metrics.histogram("portal.workspace.launch_duration")
               .description("Time from request to ready")
               .unit("ms")
               .build();

    private static final LongUpDownCounter ACTIVE_WORKSPACES =
        Metrics.upDownCounter("portal.workspace.active").build();

    @Rpc
    public CompletionStage<Workspace> launchWorkspace(CallContext ctx, LaunchRequest req) {
        // Build the shared attribute set ONCE at handler entry; reuse for every emit.
        // Skips re-walking the map and re-encoding UTF-8 on each metric update.
        Attributes attrs = Attributes.of(RECIPE, req.recipe());

        return Tracing.withSpan("workspace.launch", attrs, span -> {
            long startNs = System.nanoTime();
            return doLaunch(req).thenApply(workspace -> {
                WORKSPACES_CREATED.add(1, attrs);
                LAUNCH_DURATION.record(
                    (System.nanoTime() - startNs) / 1_000_000.0,
                    attrs.toBuilder().put(BOUNDARY, workspace.boundary()).build());
                ACTIVE_WORKSPACES.add(1, attrs);

                span.setAttribute("workspace.id", workspace.id());
                return workspace;
            });
        });
    }
}
```

Two patterns the Java DX leans into:

1. **Interned `AttributeKey<T>` constants.** Declaring keys once at class load and reusing them lets the binding cache the UTF-8-encoded byte form, so the FFI hot path skips encoding entirely.
2. **Attributes built at handler entry, reused for every emit.** Building once outside `thenApply` and reusing across counter / histogram / span calls compounds the savings — three FFI calls share one allocation.

The win is not a new API. The win is that **the SDK and exporter live in one place** and every binding gets them automatically.

---

## How it works under the hood

```
┌────────────── User code (Python / TS / Java / Kotlin) ───────────────┐
│  WORKSPACES_CREATED.add(1, {recipe})                                  │
│       │                                                               │
│       │  binding's Counter wrapper                                    │
│       │  - holds opaque handle to Rust-side instrument                │
│       │  - converts attrs (PyDict / JS object / Map) → KV array       │
│       │  - merges in extractor-derived attributes from CallContext    │
│       ▼                                                               │
└───────│───────────────────────────────────────────────────────────────┘
        │  one FFI call (cheap: atomic increment / bucket selection)
        ▼
┌──────── core (aster-otel crate) ────────────────────────────────────────┐
│  opentelemetry::metrics::Counter::add(value, attributes)                │
│       │                                                                 │
│  built-in core metrics call the SAME meter directly (no FFI)            │
│       │                                                                 │
│       ▼                                                                 │
│  PeriodicReader → OTLP gRPC/HTTP exporter → operator's collector        │
└─────────────────────────────────────────────────────────────────────────┘
```

Per-update cost: one FFI call + one atomic increment (counters / up-down counters / gauges) or one bucketed bin (histograms). Aggregation, encoding, and export happen on a Rust background task on the SDK's normal cadence (default 60s). For very hot metrics (>10 k/sec from one Python process), the binding wrapper can pre-aggregate per attribute-set and flush in batches; not a Day-1 concern.

---

## Export — push (default) vs pull (Prometheus)

The default export path is OTLP **push** and does not need an HTTP server in the Aster process. Prometheus **pull**-based scraping does, and that is where a dedicated ops-endpoints HTTP listener comes in.

### Push: OTLP, no in-process server

The Rust SDK's `opentelemetry-otlp` exporter opens an outbound gRPC or HTTP/protobuf connection to the operator's OTel collector or observability backend. Aster is purely a client. Most modern observability stacks — Honeycomb, Datadog, Grafana Cloud, the upstream OTel Collector, anything OTel-native — accept OTLP push directly.

This is the Day-0 zero-server case. No listening port, no firewall change, no TLS cert to provision. The operator points `OTEL_EXPORTER_OTLP_ENDPOINT` at their collector and it works.

### Pull: Prometheus scrape, served by `aster-ops-endpoints`

Some operators run Prometheus and want the classic `GET /metrics` exposition endpoint. That requires an HTTP listener in-process, which converges with two existing threads on the roadmap:

1. **HealthServer HTTP migration into core** (rust-core-migration plan, 2026-04-20) — `/healthz`, `/readyz`.
2. **HTTP/3 transport on Salvo** (`working_ideas/aster-http3-transport.md`) — Salvo is already chosen as the framework, sharing the `noq` QUIC stack with the Iroh transport via `noq-h3-listener`.

The convergence: one Salvo-based **ops-endpoints surface in core**, feature-gated, mounting whichever endpoints the operator opts into.

### Configuration

Programmatic example. Operators only enable what they need:

```python
from aster import AsterServer, Observability, PrometheusConfig, HealthConfig

server = AsterServer(
    services=[PortalService()],
    observability=Observability(
        otlp_endpoint="https://otel.portal.io:4317",      # push, default
        # Opt-in: Prometheus scrape (feature = "prometheus")
        prometheus_scrape=PrometheusConfig(
            bind="0.0.0.0:9100",
            path="/metrics",
        ),
        # Opt-in: health / readiness (feature = "health")
        health_endpoints=HealthConfig(
            bind="0.0.0.0:9100",   # share port with prometheus_scrape
            liveness="/healthz",
            readiness="/readyz",
        ),
    ),
)
```

Behaviour:

- If the HTTP/3 transport is configured, ops endpoints **mount on the same Salvo `Server`** (same port, same TLS cert) as a routing detail. No second port to firewall.
- If the HTTP/3 transport is not configured, `aster-ops-endpoints` binds its own minimal Salvo listener on the configured ports.
- If neither `prometheus_scrape` nor `health_endpoints` is enabled, **no HTTP server is created at all** — Salvo isn't even pulled into the dep tree.

### Crate layout

```
crates/
  aster-otel/                  # OTel SDK + OTLP exporter (always present)
    features:
      prometheus               # adds dep on opentelemetry-prometheus
                               # for the exposition-format encoder
  aster-ops-endpoints/         # Salvo-based scrape/health/admin server
                               # (feature-gated; absent unless any feature enabled)
    features:
      prometheus               # mounts /metrics handler from aster-otel
      health                   # mounts /healthz, /readyz
  aster-transport-salvo/       # HTTP/1+2+3 transport
                               # ops-endpoints reuses its Server when configured
```

Default build: just `aster-otel` with OTLP push. No HTTP server. No Salvo dep.

Operator enables the `prometheus` feature → `aster-ops-endpoints` is pulled in → small Salvo listener serves `/metrics`. If HTTP/3 transport is also on, the listener is shared.

### Why Salvo for ops endpoints, not raw hyper / axum

A lighter alternative (raw `hyper`, `axum`) could handle `/metrics` + `/healthz` in 300–500 LOC. Salvo is the right consolidation choice because:

- **Already coming for the HTTP/3 transport.** One HTTP framework in core instead of two.
- **Aster interceptors compose with Salvo handlers.** Auth-gated `/readyz`, rate-limited scrape, structured logs on admin endpoints all fall out of the existing interceptor chain.
- **Future ops endpoints are likely.** `/admin/connections`, `/admin/sessions`, dynamic config reload, debug pprof — once an HTTP listener with auth and middleware exists, adding endpoints is cheap.
- **Binary cost is acceptable.** ~1–2 MB of additional compiled size for operators who only want Prometheus scrape and skip the HTTP/3 transport. The consolidation benefit is worth it.

### Backend mode summary

| Operator setup | OTLP push | Prometheus pull | Health endpoints | HTTP listener? |
|---|---|---|---|---|
| OTel Collector / Honeycomb / Datadog / Grafana Cloud | yes | no | optional | only if health enabled |
| Prometheus + Alertmanager (classic) | optional | yes | yes | yes (one port) |
| Mimir / Cortex (push-OTLP, multi-tenant) | yes | no | yes | only if health enabled |
| Hybrid (push for traces, pull for metrics) | traces yes | metrics yes | yes | yes |

The first row is Day-0 default. The others are opt-in via features and config.

---

## Trace context across FFI — the non-obvious part

A user-created span (`tracing.span(...)`) needs to parent to the RPC's server span, which was created by core. Two pieces:

1. **When core dispatches a call into the binding**, it stores the active OTel `Context` (trace ID, span ID, baggage) in the binding-side context object — Python `contextvars`, JS `AsyncLocalStorage`, Java `Scope`. The binding's `tracing.span(...)` reads this context as the parent.

2. **When user code makes an *outbound* Aster RPC inside a handler**, the binding wrapper attaches the current span context to the outgoing call (W3C `traceparent` and `baggage` headers carried in Aster framing). The remote peer's core picks it up, populates its own context, dispatches into its handler — and the chain keeps going across nodes and across language boundaries.

Result: one trace spans `client → portal-cp → enrollment-verifier → audit-log` even when those run in different bindings on different machines. This is what the per-binding OTel SDKs *can't* coordinate today; moving the SDK to core makes it implicit.

---

## Java performance and virtual threads

Java FFI is not a single thing, and the answers depend heavily on which mechanism the binding uses. Aster's Java binding is on **Project Panama (Foreign Function & Memory API)**, confirmed in `bindings/java/aster-runtime/src/main/java/site/aster/ffi/IrohLibrary.java`. That choice changes the perf and virtual-thread story materially.

### FFI cost — Panama, not JNI

| Mechanism | Per-call overhead | Virtual-thread behaviour |
|---|---|---|
| JNI (legacy) | ~30–50 ns | Pins carrier thread for the native frame's duration |
| JNA | ~200–500 ns (reflection-heavy) | Same as JNI |
| **Panama / FFM (Aster's choice)** | **~1–5 ns for simple downcalls** | **Does not pin** — `Linker.Option.critical(true)` marks short non-blocking calls inline |

Per-emit cost breakdown for a typical `counter.add(1, attrs)`:

```
~3 ns   Panama downcall handle invocation
~30 ns  Attributes → KV array conversion (the dominant term)
~2 ns   Rust-side atomic increment
─────
~35 ns  total per metric event
```

For 1M metric events/sec across one JVM that's ~35 ms/sec — about 3.5% of one core. For typical service rates (10k–100k events/sec) it's noise.

**The 30 ns of attribute conversion dominates the FFI call itself.** This is why the Java DX above leans on interned `AttributeKey<T>` constants and `Attributes` objects pre-built at handler entry — those patterns target the actual bottleneck. For metrics emitting >500k events/sec on a single attribute set, the binding wrapper can later add thread-local pre-aggregation; cheap to add when profiling shows it, not Day 1.

### Virtual threads — three things to know

**(1) FFI calls themselves don't pin.** Panama downcalls don't enter a native frame the JVM can't unmount from. Marking the metric-update FFI methods with `Linker.Option.critical(true)` tells the JVM "this call won't block, run it inline" — perfect for atomic-increment-shaped operations on the Rust side. Virtual threads stay on their carrier for the ~3 ns the call lasts, then are free to be unmounted.

**(2) Trace context propagation uses `ScopedValue` (stable in Java 25).** `ScopedValue` is fast, virtual-thread-aware, and bounded to a lambda block — exact fit for `Tracing.withSpan(name, attrs, span -> {...})`. Aster's Java binding uses it directly:

```java
public final class Tracing {
    private static final ScopedValue<Span> CURRENT_SPAN = ScopedValue.newInstance();

    public static <T> T withSpan(String name, Attributes attrs, Function<Span, T> body) {
        Span span = tracer.spanBuilder(name).setAllAttributes(attrs).startSpan();
        try {
            return ScopedValue.where(CURRENT_SPAN, span).call(() -> body.apply(span));
        } finally {
            span.end();
        }
    }

    public static Span currentSpan() {
        return CURRENT_SPAN.orElse(NoopSpan.INSTANCE);
    }
}
```

`ScopedValue` over `ThreadLocal` is the right call on virtual-thread-heavy workloads — `ThreadLocal` allocates per-vthread storage and pays GC cost; `ScopedValue` lives on the call stack and costs nothing per-vthread.

**(3) `CompletionStage` chains break OTel context unless captured.** This is general OTel-on-Java pain, not Aster-specific. The fix lives in the binding's `Tracing.withSpan` overload for async returns:

```java
public static <T> CompletionStage<T> withSpanAsync(
        String name, Attributes attrs, Function<Span, CompletionStage<T>> body) {
    Span span = tracer.spanBuilder(name).setAllAttributes(attrs).startSpan();
    // Capture the ScopedValue carrier so the completion callback runs in the
    // span's context regardless of which carrier thread the stage resumes on.
    Context captured = Context.current().with(span);
    return ScopedValue.where(CURRENT_SPAN, span).call(() ->
        body.apply(span).whenComplete((result, error) -> {
            try (Scope ignored = captured.makeCurrent()) {
                if (error != null) {
                    span.recordException(error);
                    span.setStatus(StatusCode.ERROR);
                }
                span.end();
            }
        })
    );
}
```

Get this right once in the binding and every user benefits. Users write `Tracing.withSpanAsync(...)` and the context-capture-and-restore is invisible.

### Concrete numbers to set expectations

For the Portal control plane shape (RPC ~1 ms median, ~15 metric events per call, ~10k RPS sustained):

- **150k metric events/sec × 35 ns ≈ 5 ms/sec total CPU** for metric updates. ~0.5% of one core. Measurable but not a concern.
- **Trace overhead:** ~80 ns to start a span, ~40 ns to end it, plus per-attribute cost. ~150 ns × 10k RPS = 1.5 ms/sec. Same order, also fine.
- **Worst case (debug-everything tracing on, 100% sample):** ~5–8% CPU. Sample-rate down to 10% for production; enable 100% on demand for triage.

### Minimum runtime

Java 25+ for the Aster Java binding. Justified by:

- Panama / FFM is stable across the runtime (was stable from 22, but 25 cleans up a few `Linker.Option` corners).
- `ScopedValue` is stable in 25 (was preview earlier). Avoids carrying a `ThreadLocal` shim for older versions.
- Virtual threads have benefited from continuous tuning through 21–25; pinning issues with `synchronized` are largely resolved by 24.

This is documented as a hard requirement in the Java binding's POM and CI matrix. Customers on older runtimes use the Python or TypeScript binding (or upgrade — Java 25 is an LTS).

---

## Multi-tenant attribution — the lambda escape hatch

Aster does not have a tenant model. The rcan carries `iss / aud / sub / scope / nonce / exp`; "tenant" is a concept the downstream product (Portal, etc.) layers on top, typically by stuffing `tenant_id` into the rcan's scope payload or by reading it from a header. **Aster ships the extractor mechanism; the application supplies the lambda that knows where its tenant lives.**

```python
server = AsterServer(
    services=[PortalService()],
    observability=Observability(
        otlp_endpoint="...",
        # Aster has no opinion about what these mean.
        # Each lambda runs ONCE per RPC when the CallContext is established;
        # the result is cached on the call context and re-used by every
        # subsequent span/metric emit during that call.
        attribute_extractors={
            "tenant.id": lambda ctx: ctx.rcan_claims.get("tenant_id"),
            "user.id":   lambda ctx: ctx.rcan_claims.get("sub"),
            "plan":      lambda ctx: ctx.rcan_claims.get("plan"),
        },
        # Which extracted attributes also propagate as baggage to outbound
        # RPCs from this service. Independent of attach-to-spans.
        propagate_as_baggage=["tenant.id"],
        cardinality_cap=10_000,
    ),
)
```

### Design notes that fall out of being honest about this

1. **Lambdas run once per call, not per emit.** A handler that emits 50 metrics shouldn't re-parse rcan claims 50 times. Aster runs every extractor at call-start, stashes the dict on the call context, and built-in + auto-attr metrics read from that cache.

2. **The extractor returns `None` for "not applicable."** A service that's sometimes called by tenant-scoped rcans and sometimes by anonymous health probes shouldn't emit `tenant.id="None"` on the latter. None means "skip this attribute on this call," matching OpenTelemetry's optional-attribute semantics.

3. **Baggage propagation is opt-in per attribute, not all-or-nothing.** `tenant.id` is fine to send to downstream services. `user.email` (if you ever extracted one) probably isn't. Two independent config knobs (`attribute_extractors` and `propagate_as_baggage`).

4. **User-defined metrics opt in to auto-attrs.** Built-ins always get the extracted attributes; custom metrics declare which ones they want, because the developer should think about cardinality consciously:

   ```python
   # Auto-attached: every emit gets tenant.id from the extractor
   WORKSPACES_CREATED = metrics.counter(
       "portal.workspace.created",
       auto_attrs=["tenant.id", "deployment.env"],
   )

   # Opt-out for a hot metric where tenant cardinality is dangerous
   RAW_BYTES_PROCESSED = metrics.counter(
       "portal.bytes.processed",
       auto_attrs=[],     # no tenant — too high cardinality at this volume
   )
   ```

---

## The three places an attribute can live

OpenTelemetry separates these intentionally; using the wrong one is most of the multi-tenant pain.

| Location | Set when | Cardinality cost | Right for `tenant_id`? |
|---|---|---|---|
| **Resource attributes** | Once at SDK init | None | No — one process serves many tenants |
| **Span / metric attributes** | Per event | High — every unique combination becomes a series | Yes for spans, *carefully* for metrics |
| **Baggage** | Per request, propagates across RPCs | None on its own | Yes, as the propagation channel |

The recommendation: **baggage propagates, attributes record, collector routes.** Three layers, each doing what it is good at.

---

## When `tenant.id` on metrics is safe vs dangerous

| Backend | `tenant.id` on metrics? |
|---|---|
| Traces — Tempo / Jaeger / Honeycomb / Datadog APM | **Always.** Trace storage is per-trace, not per-series. |
| Logs — Loki / Datadog Logs / OpenObserve | **Always.** Log storage doesn't pay cardinality cost. |
| Prometheus (vanilla, single-tenant TSDB) | **Risky** above ~1000 tenants. 10 k tenants × 10 metrics × 5 labels = 500 k series per pod. |
| Grafana Mimir / Cortex (multi-tenant) | **Use `X-Scope-OrgID` instead.** Tenant becomes the storage partition, not a label. |
| Datadog / New Relic (per-series billing) | **Read your bill.** Per-series cost compounds quickly. |
| ClickHouse-backed (Honeycomb / Axiom / Coroot / Signoz) | **Safe.** High-cardinality stores were built for this. |

Default policy in Aster: emit `tenant.id` on built-in spans and logs unconditionally; emit on metrics only when the user opts the metric in via `auto_attrs`.

---

## Real multi-tenancy via the OTel Collector

If tenants need actual isolation (separate retention, alerting, RBAC), don't put `tenant.id` as a label and call it done. Run the OpenTelemetry Collector with a `routing` processor that strips `tenant.id` from the data and sets it as the `X-Scope-OrgID` header on the export:

```yaml
# otel-collector-config.yaml
processors:
  routing:
    from_attribute: tenant.id
    table:
      - value: maersk
        exporters: [otlphttp/maersk]
      - value: cma-cgm
        exporters: [otlphttp/cma-cgm]
    default_exporters: [otlphttp/shared]

exporters:
  otlphttp/maersk:
    endpoint: https://mimir.example.com/otlp
    headers: { X-Scope-OrgID: maersk }
  otlphttp/cma-cgm:
    endpoint: https://mimir.example.com/otlp
    headers: { X-Scope-OrgID: cma-cgm }
  otlphttp/shared:
    endpoint: https://mimir.example.com/otlp
    headers: { X-Scope-OrgID: shared }
```

Mimir, Cortex, Tempo, and Loki all natively understand `X-Scope-OrgID` for tenant partitioning. This gives proper isolation: per-tenant retention policies, query-time auth, billing, and per-tenant alerting rules. The application keeps emitting one stream; the collector fans out.

The Aster application doesn't need to know about any of this — it emits with `tenant.id` as an attribute, and the collector translates. If the customer later swaps Mimir for ClickHouse-backed storage, the application stays the same and only the collector config changes.

---

## Cardinality control

Custom-metric attributes are user-supplied and can blow up if someone labels by `user_id`, `request_id`, or anything similarly unbounded. Without a guard, one bad label kills the TSDB.

`Observability(cardinality_cap=N)` is enforced in the Rust SDK: per-instrument tracking of unique attribute combinations, drop-and-log anything beyond N with a structured log so the offending metric/attribute is findable fast:

```
WARN aster.otel.cardinality_exceeded
  metric=portal.bytes.processed
  cap=10000
  dropped_attrs={"request_id":"abc-123-def-..."}
  hint="Consider removing high-cardinality attributes (request_id, user_id, raw_paths) or opt out via auto_attrs=[]"
```

Per-instrument override available for a metric that legitimately needs a higher cap (`metrics.counter("foo", cardinality_cap=50_000)`).

---

## Logs — small helpers, not a pipeline

Logs are different from metrics and traces. Every Aster binding language already has an entrenched logging framework, and in modern deployments the logs themselves don't even leave the application via the application — the runtime collects stdout and a node-level agent forwards it.

| Language | Dominant logger | Where logs go in production |
|---|---|---|
| Java / Kotlin | SLF4J + Logback / Log4j2 | stdout (JSON) → fluent-bit / vector / promtail / datadog-agent |
| Python | stdlib `logging`, `structlog`, `loguru` | stdout (JSON) → same |
| TS / Node | `pino`, `winston`, `bunyan` | stdout (JSON) → same |

Aster doesn't get to live in this pipeline and shouldn't try. The application's role is "write structured JSON to stdout"; the cluster handles transport. **Aster's job here is the smallest possible one: expose the hooks each logger framework needs to enrich log lines with trace and tenant context, and document how to wire them in.**

### The hooks Aster exposes

A binding-side `aster.tracing` (or equivalent) module exposes these primitives. Loggers consume them; Aster doesn't own the logger.

| Hook | Returns | Used by |
|---|---|---|
| `current_trace_id()` | `Option<str>` (16-byte trace ID hex, or None outside a span) | Every logger framework's "context" mechanism |
| `current_span_id()` | `Option<str>` (8-byte span ID hex, or None) | Same |
| `current_baggage()` | `dict[str, str]` of all baggage entries | For pulling tenant.id, user.id, etc. — exact keys are the application's choice |
| `with_trace_context()` | A scoped helper that activates a context (rarely needed by user code) | Edge cases — async work that crosses a boundary the binding can't auto-instrument |

These four functions are the entirety of the API surface. Anything more sophisticated (filters, processors, appenders, transports) is documentation that wires these into the dominant logger of each language.

### Per-language configuration recipes

Each binding ships a one-page doc showing the canonical wiring for the dominant logger(s). The user copies a block once at startup; nothing in the application's logging calls changes.

#### Java / Kotlin — Logback MDC

`Tracing.withSpan(...)` populates Logback's MDC with three keys: `trace_id`, `span_id`, and any baggage entries (e.g. `tenant.id`, `user.id`).

```xml
<!-- logback.xml -->
<configuration>
  <appender name="STDOUT" class="ch.qos.logback.core.ConsoleAppender">
    <encoder class="net.logstash.logback.encoder.LogstashEncoder">
      <includeMdcKeyName>trace_id</includeMdcKeyName>
      <includeMdcKeyName>span_id</includeMdcKeyName>
      <includeMdcKeyName>tenant.id</includeMdcKeyName>
    </encoder>
  </appender>
  <root level="INFO">
    <appender-ref ref="STDOUT"/>
  </root>
</configuration>
```

Application code is unchanged: `log.info("workspace launched")` automatically carries the three fields. For Log4j2 the equivalent is `ThreadContext` + `JsonTemplateLayout` with the MDC-include pattern.

#### Python — stdlib `logging` filter or `structlog` processor

stdlib `logging`:

```python
import logging
from aster.tracing import LoggingFilter   # injects trace_id, span_id, baggage

handler = logging.StreamHandler()
handler.setFormatter(logging.Formatter('%(message)s'))   # use a JSON formatter in prod
handler.addFilter(LoggingFilter())
logging.getLogger().addHandler(handler)
logging.getLogger().setLevel(logging.INFO)

log = logging.getLogger("portal")
log.info("workspace launched", extra={"workspace_id": ws.id})
# → {"msg": "workspace launched", "workspace_id": "...",
#    "trace_id": "...", "span_id": "...", "tenant.id": "..."}
```

`structlog`:

```python
import structlog
from aster.tracing import structlog_processor

structlog.configure(
    processors=[
        structlog_processor,                     # ← Aster's hook
        structlog.processors.TimeStamper(),
        structlog.processors.JSONRenderer(),
    ],
)
```

`loguru`:

```python
from loguru import logger
from aster.tracing import loguru_patcher

logger.configure(patcher=loguru_patcher)         # ← Aster's hook
```

#### TypeScript / Node — pino mixin / winston format

`pino`:

```typescript
import pino from 'pino';
import { tracingMixin } from '@aster/aster';

const log = pino({ mixin: tracingMixin });        // ← Aster's hook
log.info({ workspaceId: ws.id }, 'workspace launched');
// → {"msg":"workspace launched","workspaceId":"...",
//    "trace_id":"...","span_id":"...","tenant.id":"..."}
```

`winston`:

```typescript
import winston from 'winston';
import { winstonTracingFormat } from '@aster/aster';

const log = winston.createLogger({
  format: winston.format.combine(
    winstonTracingFormat(),                       // ← Aster's hook
    winston.format.json(),
  ),
  transports: [new winston.transports.Console()],
});
```

Each helper is roughly 30–60 LOC and wraps the four primitives. Total binding-side surface is small.

### The optional OTLP-logs bridge

Some operators *do* want OTLP for everything (traces + metrics + logs to one endpoint, no node agent). For them, an opt-in `aster-otel-logs` feature provides:

- A Logback / Log4j2 appender (Java)
- A `logging.Handler` (Python)
- A pino `transport` and a winston `Transport` (TypeScript)

…that forwards records to the Rust-side OpenTelemetry logs SDK and exports via OTLP alongside traces and metrics. Off by default. When it's worth it:

| Setup | OTLP logs bridge worth it? |
|---|---|
| k8s with fluent-bit / vector / promtail (most operators) | No — node agent already does this |
| Bare VM / non-container deploy with no node agent | Maybe |
| Single-vendor stack (Honeycomb, Datadog) wanting consolidation | Yes |
| Compliance setup requiring in-process audit trail | Yes |
| Edge / IoT with no node agent | Yes |

### What Aster ships vs doesn't

| Concern | Aster ships? |
|---|---|
| Trace + baggage hooks for the dominant logger in each binding | **Yes** — four primitives + per-framework wrapper helpers |
| Documented one-page wiring guide per language / framework | **Yes** — Logback, Log4j2, stdlib logging, structlog, loguru, pino, winston |
| A new logging API users have to learn | **No** — they keep using their framework |
| In-process log forwarding to OTLP | **Optional**, feature-gated, off by default |
| Log routing, filtering, sampling, rate-limiting | **No** — logger's and node agent's job |
| Log file management, rotation, retention | **No** — `journald` / `logrotate` / k8s handle this |
| Log redaction / PII scrubbing | **No** — application's choice; document that baggage values flow into log lines so don't put secrets in baggage |

### One important caveat to document

Because baggage values land in log lines via the hooks above, **anything in baggage is effectively logged.** Don't put secrets, raw PII, or session tokens in baggage. The `propagate_as_baggage=[...]` config in `Observability` should be paired with a "fields baggage may carry" review during code review.

### Beyond stdout — the decentralized log distribution path

The above covers "make sure the language-native logger has trace context, then let stdout + the cluster's node agent do the rest." That answer fits centralized SaaS-on-k8s deployments. It does **not** fit Aster's other deployment shapes — customer-owned VMs with no node agent, laptops, edge devices, P2P meshes, air-gapped enterprises.

For those, see [decentralized-log-distribution.md](decentralized-log-distribution.md), which proposes an opt-in `AsterLogAppender` per binding (same standard appender mechanism each logger framework already exposes — *not* a new logging framework) that routes log records via iroh-blobs + iroh-gossip to any number of subscribers (control-plane live tail, S3 archiver, SIEM bridge, peer node). The Aster runtime is already in the application's process for RPC, so the publisher runs on the existing tokio runtime — no sidecar.

The two paths coexist: an application can wire its logger to write to *both* stdout (for the cluster case) *and* the AsterLogAppender (for the decentralized case). They are not mutually exclusive.

---

## Migration / what this replaces

- Drop `bindings/{python,typescript,java}/...interceptors/metrics.{py,ts,java}`.
- Replace with thin `aster.metrics` / `aster.tracing` modules per binding (each ~100 LOC, mostly type stubs over the FFI surface).
- The user-facing `MetricsInterceptor()` in server config becomes a no-op shim (kept for back-compat) — built-in metrics are on by default once `Observability(...)` is configured.
- New `aster-otel` crate in core: ~500 LOC wrapping `opentelemetry`, `opentelemetry-otlp`, `opentelemetry-sdk`, with a small FFI surface for binding-side instrument handles.
- `CallContext` gains a per-call extractor cache and a baggage map.

Backwards compat stance: existing services that only used `MetricsInterceptor()` keep working with no change. Services that explicitly imported `opentelemetry` in their own code keep working — the binding-side OTel SDK still functions; it just no longer ships as a hard dependency or competes with the core SDK on configuration.

---

## What's still open

1. ~~**Logs.**~~ Resolved by §"Logs — small helpers, not a pipeline" — Aster exposes four primitives (`current_trace_id`, `current_span_id`, `current_baggage`, `with_trace_context`) plus per-framework wrappers (Logback / Log4j2 / stdlib logging / structlog / loguru / pino / winston). Per-language one-page wiring docs. No core-side logs pipeline by default. Optional `aster-otel-logs` feature for operators who want OTLP consolidation.

2. **Exemplars.** OpenTelemetry histograms can carry exemplar trace IDs, which makes "click metric → see traces" work. Worth wiring on Day 1 since core sees both the metric emit and the active trace context. Gap analysis: most existing TSDB UIs don't surface exemplars yet, but Mimir + Grafana do.

3. ~~**Prometheus scrape endpoint.**~~ Resolved by §"Export — push (default) vs pull (Prometheus)" — feature-gated `aster-ops-endpoints` crate built on Salvo, shared with HealthServer migration and HTTP/3 transport's Salvo `Server` when present.

4. **`tenant_id` propagation across non-Aster boundaries.** When a Portal handler calls AWS S3 or a vanilla HTTPS service, baggage isn't auto-attached. Document the pattern (read baggage manually, set on outbound headers) and provide a helper in the binding's `tracing` module.

5. **Sampling-with-tenant-context.** "Sample 100% for tenant T's traces, 1% for everyone else" is a real ask. OpenTelemetry's `ParentBasedSampler` plus a custom `Sampler` reading baggage should compose, but worth a worked example in the doc.

6. **Cost of attribute extraction at very high RPC rates.** Lambdas are cheap but not free. Profile on a 100k-RPS service before declaring victory; provide a "static extractors" fast-path for cases where the value is the same for the whole process.

7. **Per-binding OTel SDK still installed?** A user who *also* wants to instrument their own non-Aster code (e.g. a SQLAlchemy query) still needs the Python OTel SDK. Aster's core SDK doesn't replace that; documentation should make clear that "Aster emits via core; your own code emits via your language's SDK; both end up at the same collector via the OTLP endpoint." Two SDKs in one process is fine — they don't fight.

### Resolved during design

- **Java minimum runtime:** Java 25+ (locked in). Lets the Java binding rely on stable Panama / FFM and stable `ScopedValue` directly, no compatibility shims. Documented as a hard requirement in the binding's POM and CI matrix.
- **Java FFI mechanism:** Project Panama (already what `IrohLibrary.java` uses). JNI / JNA explicitly out — perf and virtual-thread compatibility both benefit.
- **Virtual-thread compatibility:** validated in §"Java performance and virtual threads" — `ScopedValue` for context, `Linker.Option.critical(true)` on hot-path FFI, `Tracing.withSpanAsync` for `CompletionStage` chains.
- **Prometheus scrape / health endpoints:** feature-gated `aster-ops-endpoints` crate built on Salvo. Shared with the HealthServer-migration plan (`project_rust_core_migration`) and the HTTP/3 transport's Salvo `Server` (`working_ideas/aster-http3-transport.md`) when configured. OTLP push remains the Day-0 default with no in-process HTTP listener.
- **Logs:** explicit non-goal to ship a logs pipeline. Aster exposes four primitives + per-framework wrappers; the application's logger framework owns log emission, the node agent owns transport. Optional `aster-otel-logs` feature for OTLP consolidation when the operator wants it. Caveat documented: baggage values land in log lines, so do not put secrets in baggage.

---

## Mental model

> Aster's core owns the SDK and the exporter. Bindings hand out cheap instrument handles. The application supplies lambdas for the attributes Aster has no business knowing about (tenant, plan, user). The collector handles real isolation when isolation matters.
>
> One pipeline. One configuration. Custom metrics in any binding. Trace context that survives FFI by construction. Multi-tenancy that is honest about who knows what.
