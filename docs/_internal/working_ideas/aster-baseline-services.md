# `aster.*` Baseline Services — A Standard Ops Surface on Every Node

> **Status: working idea, not a spec.** Everything below is exploratory — the catalog, the names, the auth model, the RPC shapes. Expect refinement and scope reduction before any of this becomes normative. Don't take a single name or method signature here as committed; we will likely ship a smaller, sharper subset than what's enumerated.

**Date:** 2026-05-06
**Companion docs:**
- [decentralized-log-distribution.md](decentralized-log-distribution.md) — the publisher/subscriber substrate; this doc adds the on-demand pull side
- [observability-in-core.md](observability-in-core.md) — OTel SDK, metrics, traces, hooks
- [identity-worked-example.md](retired.identity-worked-example.md) — tenant model + rcan scopes that gate access to the baseline services
- [trust-discovery-thinking-session.md](retired.trust-discovery-thinking-session.md) — handle registry / discovery primitives the baseline `discovery.*` services would expose

---

## Why this doc exists

Several earlier threads keep wanting the same thing without naming it: a uniform set of services every Aster node exposes, that operators (and tooling, and control panels) can rely on existing on every node regardless of which downstream product runs there. Health endpoints. Streaming logs. A way to introspect connections. A way to see what contracts a node serves. Profile dumps when something is slow.

The rule of thumb: if a question of the form "tell me about this node" or "show me what's happening inside this node" comes up across more than one downstream product, it probably belongs to the baseline.

We've reserved the `aster.*` namespace for exactly this purpose. This doc proposes what to put there.

The proposal is deliberately ambitious in shape. **Final scope will be smaller.** The point of writing it broadly is to see the surface as a whole before deciding what's actually Day 1 vs Day N vs never.

---

## The pattern — prior art that works

| System | Surface | Property worth borrowing |
|---|---|---|
| gRPC | `grpc.reflection.v1.ServerReflection` | One mandatory built-in service every server exposes; lets any client introspect any server |
| Kubelet | `/healthz`, `/metrics`, `/api/v1/pods`, `/debug/pprof` | Operators learn the surface once; it exists on every kubelet |
| Tailscale | local API socket (`tailscale status`, `debug`) | Uniform ops surface across all device types |
| libp2p | `/ipfs/identify/1.0.0`, `/ipfs/ping/1.0.0` | Discoverable identity / liveness primitives over the same transport as everything else |
| Cosmos SDK | `cosmos.bank.*`, `cosmos.staking.*` modules | Standard module namespaces every chain exposes |

Common threads: **uniform, discoverable, scope-gated, shipped with the runtime.** The application can't disable them and can't override them. That's the property that makes operator tooling possible — every control panel, every CLI, every monitor knows it can call these RPCs and get a predictable response.

---

## Namespace reservation

Strict rule: `aster.*` is reserved for baseline services defined in core. Bindings cannot define `aster.*` services themselves. A downstream product that wants its own ops surface lives in its own namespace (`portal.ops.*`, etc.).

Versioning: probably `aster.ops.v1.Logs` style in the contract path, with the contract-identity hash giving the more granular guarantee. Cosmetic question — defer.

---

## Proposed catalog

A first-cut list, grouped by concern. Treat this as the **maximum** surface; everything is up for being cut.

### Identity & discovery

```
aster.ops.NodeInfo          # version, build, capabilities, public endpoint id, uptime
aster.ops.Manifest          # which contracts this node serves (reflection)
aster.discovery.Resolve     # handle → anchor pubkey → endpoint list
                            # (the registry side of §"Control-plane HA"
                            # in retired.identity-worked-example.md)
```

### Health & lifecycle

```
aster.ops.Health            # liveness, readiness, dependency status, last-error
aster.ops.Lifecycle         # graceful shutdown, drain mode (rolling deploys)
```

### Live observability — the "connect and see" surface

```
aster.ops.Logs              # stream recent + live logs from local buffer
aster.ops.Metrics           # snapshot or stream local metric values
aster.ops.Traces            # stream recent local trace records (sampled)
```

**All three take a filter parameter.** The filter language is intentionally not specified here — it'll be its own design session. For now assume something with the expressiveness of "structured WHERE clause over attribute keys" (severity, service, trace_id, baggage values like tenant.id, time range). We're choosing between shapes like:
- An OTel-style attribute matcher (simple equality + range, easy to implement, limited)
- A subset of CEL (rich, well-defined, library overhead)
- A custom DSL (avoid)
- PromQL/LogQL-shaped (familiar to operators but tied to those ecosystems)

Whatever the filter language ends up being, **it should be the same across Logs, Metrics, and Traces** — operators learn one filter syntax for all three. Different Aster ops services should not have different query languages.

### Debug & inspection (gated by scope)

```
aster.ops.Connections       # list active QUIC connections, sessions, by peer
aster.ops.Config            # read-only config dump (with secrets redacted)
aster.ops.Profile           # CPU / heap / allocation profiles (pprof-shaped)
aster.ops.Debug             # controlled introspection — feature flags, stack dumps
```

`Profile` is the highest-effort to implement portably (each language runtime has its own profiling story); easy candidate for scope reduction or deferral.

### Trust subsystem

```
aster.trust.Tokens          # admission token verification (called by other peers)
aster.trust.Anchor          # anchor delegation lookup
aster.trust.Enrollment      # the EnrollmentProof verifier surface
                            # (see retired.identity-worked-example.md §"Pluggable
                            # enrollment-proof pattern")
```

These are the most likely to actually be normative — they're the ones other Aster nodes call as part of the protocol, not just operators. May graduate from "baseline" to "core protocol" if needed.

---

## The streaming-logs RPC concretely

The other "connect and see" services will follow the same shape with their own filter / record types. Logs is the worked example because it's the one driving the design.

```protobuf
service aster.ops.Logs {
  // Recent buffer (last N seconds) then live tail. Server-streaming.
  rpc Tail(TailRequest) returns (stream LogRecord);

  // One-shot bounded fetch from local buffer.
  rpc Fetch(FetchRequest) returns (FetchResponse);

  // Status of the local log buffer (depth, oldest entry, dropped counters).
  rpc Status(StatusRequest) returns (LogBufferStatus);
}

message TailRequest {
  // Replay window before live tail begins.
  optional uint64 since_unix_ms;

  // Hard limit on records emitted; if absent + follow=true, stream forever.
  optional uint64 max_records;

  // Continue streaming after replay is exhausted.
  optional bool follow;

  // Filter — language TBD; placeholder shape only.
  optional Filter where;
}

message FetchRequest {
  // Bounded historical query — both start and end required.
  uint64 since_unix_ms;
  uint64 until_unix_ms;
  uint64 max_records;
  optional Filter where;
}

message LogRecord {
  uint64 ts_unix_nanos;
  Severity severity;
  string service;
  string message;
  map<string, Value> attributes;  // includes trace_id, span_id, tenant.id, etc.
  optional string trace_id;       // pulled out for indexing convenience
  optional string span_id;
}

message Filter {
  // PLACEHOLDER. Real shape in a follow-up design session.
  // Expected to support: equality on attribute keys, severity ≥ X,
  // time ranges already covered above, possibly basic AND/OR.
  string placeholder = 1;
}
```

The control-panel UX falls out:

```
Operator opens "tail logs" for node N in tenant T's dashboard.
   │
   ▼
Control plane dials node N (Aster QUIC, ed25519-mTLS-equivalent).
   │
   ▼
aster.ops.Logs.Tail({
  since_unix_ms: now - 60_000,        // last minute of history
  follow: true,                       // then keep streaming
  where: <filter: tenant.id == T>
})
   │
   ▼
Server-streaming response → WebTransport → browser → live UI.
```

No agent. No log-shipping setup. No port to open. Operator authenticated to the control plane with an rcan scoped to tenant T; the control plane dials the node carrying that rcan; the node's `aster.ops.Logs` handler verifies the rcan grants the required scope; logs stream back. **Day-0 magic moment** — the demo this whole architecture earns.

---

## Composition with decentralized log distribution

The publisher-side architecture from `decentralized-log-distribution.md` doesn't change. The local buffer (SlateDB / rotating Parquet) becomes a **shared substrate** that two consumers read from:

```
Application's logger
       │
       ▼
AsterLogAppender → local buffer (in core)
                          │
            ┌─────────────┴─────────────┐
            ▼                           ▼
   Background publisher           aster.ops.Logs handler
   (blob + gossip,                (server-streaming RPC,
   fleet-wide subscribers)        single-node tail)
```

Same buffer, two readers. They serve different access patterns:

| Access pattern | Right path |
|---|---|
| "Show me everything across the fleet, always" | gossip subscriber (the distribution doc) |
| "Tail this one specific node, right now" | aster.ops.Logs.Tail() (this doc) |
| "Search logs from 3 days ago for tenant T" | DuckDB query against S3 archive |
| "Live UI in control panel for one tenant" | gossip subscriber + UI |
| "SRE just paged, drop into one specific pod" | aster.ops.Logs.Tail() |
| "Replay last 60s before the gossip-side starts" | aster.ops.Logs.Tail(since_ms=...) |

The pull RPC means a control panel never has to wait for a gossip announcement to reach a tail subscriber — it can dial directly when the operator wants immediacy. The gossip side handles the always-on, fleet-wide case.

The same composition applies to Metrics and Traces:
- `aster.ops.Metrics.Snapshot()` reads from the in-process OTel meter directly (no buffer needed — values are already there).
- `aster.ops.Metrics.Stream()` follows the meter's reader cadence and emits deltas.
- `aster.ops.Traces.Tail()` reads from a small in-memory ring of recent spans (separate from the OTLP exporter — the exporter is push, this is pull).

---

## Auth model — scope-based (sketch)

Each baseline service declares which rcan scope it requires. The application can never override the scopes — they're fixed in core, which is what makes the surface uniform across deployments.

| Service | Required scope | Default visibility |
|---|---|---|
| `Health.liveness` | none (anonymous) | Anyone |
| `Health.readiness`, `NodeInfo`, `Manifest` | `aster.ops.read` | Anyone connected with admission |
| `Logs`, `Metrics`, `Traces` (read) | `aster.ops.observability.read` | Operator + tenant-scoped callers |
| `Connections`, `Config` | `aster.ops.inspect` | Operator only |
| `Profile`, `Debug` | `aster.ops.debug` | Operator only, time-limited tokens recommended |
| `Lifecycle.shutdown`, `Lifecycle.drain` | `aster.ops.admin` | Operator only |
| `trust.Tokens.verify` | none (anonymous) — cryptographic check | Anyone (it's a verifier) |
| `trust.Anchor.lookup` | none | Anyone |
| `trust.Enrollment.register` | depends on proof type | Per the enrollment design |

The tenant scoping property is what makes "the operator can see all tenants; tenant T's CLI can only see tenant T" work without tying the application to any particular tenant model — the rcan carries the scope, the baseline service checks it, the application's tenant interpretation (via the lambda from `observability-in-core.md` §"Multi-tenant attribution") drives the filter.

This is a sketch. Real scope names and the verification rules need a dedicated session.

---

## Open questions

1. **Mandatory vs optional services.** `NodeInfo` and `Health` are clearly mandatory. `Profile` requires hooking the language runtime (jvm-pprof, py-spy, etc.) — does every binding ship it Day 1, or do nodes advertise capability via `Manifest`?

2. **Filter language.** Shared across Logs / Metrics / Traces. Choosing between an OTel-style attribute matcher (simple, limited), a CEL subset (rich, library overhead), PromQL/LogQL-shaped (familiar but ecosystem-tied), or a custom DSL (probably avoid). Worth its own design session.

3. **Backpressure for streaming endpoints.** A slow consumer should slow the producer via QUIC stream credits without dropping data; if the buffer overflows, drop with a structured "you fell behind" event, never silently.

4. **Local-buffer-only vs cross-node aggregation.** Baseline services scope themselves to *this one node*. Cross-node aggregation is a higher-layer concern (control plane fans out, or subscribes to gossip). Keeping the built-ins single-node keeps them tractable.

5. **What happens before the appender is wired.** A node where AsterLogAppender wasn't configured has an empty buffer. `aster.ops.Logs.Status()` should surface "unconfigured" cleanly so the control panel can show "this node has no Aster log capture wired" rather than appearing broken.

6. **Coexistence with stdout-only deployments.** A user who logs only to stdout has no `aster.ops.Logs` surface. Optional `stdin-tail` mode that captures process stdout into the buffer is appealing but messy (fd inheritance, container PID 1, multi-process apps, log rotation interactions). Probably defer; document the appender as the supported path.

7. **Naming.** "Baseline services," "platform services," "ops surface," "aster baseline," "first-party services" — none feel perfect. Pick before the namespace becomes load-bearing in user docs.

8. **Versioning strategy.** `aster.ops.v1.*` vs trusting the contract-identity hash. The hash gives precise pin-of-implementation; v1/v2 in the path gives operators the human-readable "I'm targeting v1 of the surface." Probably both, with v-in-path being a soft convention.

9. **Discoverability.** A new control panel visiting an unknown node should be able to ask "which baseline services do you implement?" and get a list. `aster.ops.Manifest.ListBaselines()` or similar — or fold into general `Manifest`.

10. **Per-binding implementation effort.** Most of these are core-side (Rust handlers reading from core-owned state). `Profile` is the outlier — needs per-binding hooks. That alone is a strong argument for making `Profile` optional.

---

## Minimum viable surface vs full vision

The full catalog above is what the surface *could* look like at maturity. What's actually the minimum to ship?

**Genuine Day-1 candidates** (small, high-value, mostly core-side):

```
aster.ops.NodeInfo          # trivial; one method
aster.ops.Health            # the HealthServer migration is already happening anyway
aster.ops.Manifest          # already implicit in contract identity
aster.ops.Logs              # the killer demo; reads core-owned buffer
```

**Should-have soon**:

```
aster.ops.Metrics           # rides the OTel SDK we're already building
aster.ops.Connections       # iroh already exposes this internally; thin wrapper
aster.ops.Lifecycle         # graceful shutdown matters from the first deploy
```

**Worth scoping but probably not Day 1**:

```
aster.ops.Traces            # value depends on filter language being good
aster.ops.Config            # only useful once there's a non-trivial config surface
aster.ops.Profile           # per-binding effort; defer
aster.ops.Debug             # define-as-needed
aster.discovery.Resolve     # blocked on handle registry being live
aster.trust.*               # ride whatever the trust spec settles on
```

If the goal is "smallest surface that delivers the Day-0 control-panel-tail demo," it's: `NodeInfo` + `Health` + `Manifest` + `Logs`. Everything else is incremental.

---

## What this doc is NOT trying to be

- **Not a spec.** Names, scopes, and method signatures are illustrative. Expect them to change.
- **Not a commitment to ship the whole catalog.** Realistic delivery is the minimum-viable set above; the rest is the design vocabulary so we can talk about what comes later.
- **Not the final answer on the filter language.** That's its own session — explicitly flagged as an open question.
- **Not the final answer on auth scopes.** The table is a sketch to make the model concrete; real scope names and verification rules need to be ratified against the trust spec.
- **Not a replacement for `decentralized-log-distribution.md`.** The two compose; `aster.ops.Logs` is the pull side, the gossip path is the push side. Both read from the same local buffer.

---

## Mental model

> Every Aster node exposes the same baseline surface. A control panel, a CLI, a monitor knows it can dial any node and call the same ops RPCs and get a predictable response. The application can never disable or override these — they're shipped with the runtime, gated by rcan scopes, and namespace-reserved in `aster.*`.
>
> The "connect and see" surface (Logs, Metrics, Traces) takes a filter parameter so the operator can drill down without dragging back the whole stream. The pull RPCs and the gossip-based push substrate share a single in-process buffer in core, so an operator gets immediacy when they want it and durability when they want that.
>
> Everything in this doc is an idea, not a spec. The shipping surface will be smaller. The point of writing the full vision is to make the smaller version a deliberate cut, not a default.
