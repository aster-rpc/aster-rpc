# `aster.*` Baseline Services — A Standard Ops Surface on Every Node

> **Status: first slice SHIPPED (2026-07-02), rest is proposal.** Shipped in Rust: the compile-time `aster.*` namespace guard, union support in `#[derive(AsterType)]`, the `aster/Value`+`aster/Attr`+`aster/NodeIdentity` payload types, and `aster.ops.NodeInfo` served **by default** on every `AsterServer` (`aster/src/rpc/baseline.rs`; end-user doc [../aster-baseline-services-getstarted.md](../aster-baseline-services-getstarted.md)). Everything else below — Health, Manifest, Logs, the auth-scope model, the wider catalog — remains exploratory; expect refinement and scope reduction before it becomes normative.

**Companion docs:**
- [working_ideas/decentralized-log-distribution.md](working_ideas/decentralized-log-distribution.md) — the publisher/subscriber substrate; this doc adds the on-demand pull side
- [working_ideas/observability-in-core.md](working_ideas/observability-in-core.md) — OTel SDK, metrics, traces, hooks
- [aster-network-topology.md](aster-network-topology.md) — locality self-mapping; would surface as `aster.net.Topology` alongside `aster.ops.Connections`
- [aster-sealed-grants.md](aster-sealed-grants.md) — per-recipient capability distribution (from portal-sync); shipped as `aster::grants`; any runtime surface would land in `aster.trust.*`
- identity-worked-example.md (deleted 2026-06-10, in git history) — tenant/rcan model; superseded by directory roles per [trust-directory.md](trust-directory.md)
- trust-discovery-thinking-session.md (deleted 2026-06-10, in git history) — handle registry / discovery primitives the baseline `discovery.*` services would expose; concluded in [trust-directory.md](trust-directory.md)

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

**ENFORCED (shipped 2026-07-02).** Both macros reject the reserved namespace at compile time — `#[aster::service(name = …)]` for service names and `#[aster(wire = "ns/Name")]` for payload packages (`aster-macros/src/lib.rs`, `is_reserved_namespace`). The check covers `aster` exact plus `aster.`/`aster/` prefixes, is deliberately case-exact, and does **not** match lookalikes (`asterix`, `aster-foo`). Escape hatch for the framework itself: expansion inside the `aster` crate (`CARGO_PKG_NAME == "aster"`) is exempt, which is where `rpc::baseline` defines the real baseline types. Note this is a foot-gun guard, not a security boundary — a determined consumer can hand-implement `AsterType` — but it prevents accidental squatting.

Note the two naming forms in play: service *names* are arbitrary strings (`aster.ops.NodeInfo`), while payload types carry a `#[aster(wire = "…")]` tag that splits on the **last** `/` into `(package, name)` — so a baseline payload is `aster/NodeIdentity`, package `aster`. The reservation applies to both.

Versioning: probably `aster.ops.v1.Logs` style in the contract path, with the contract-identity hash giving the more granular guarantee. Cosmetic question — defer.

---

## Proposed catalog

A first-cut list, grouped by concern. Treat this as the **maximum** surface; everything is up for being cut.

### Identity & discovery

```
aster.ops.NodeInfo          # version, build, capabilities, public endpoint id, uptime → NodeIdentity
aster.ops.Manifest          # which contracts this node serves (reflection)
aster.discovery.Resolve     # handle → anchor pubkey → endpoint list
                            # (the registry side of §"Control-plane HA"
                            # in the deleted identity-worked-example.md; see trust-directory.md)
```

**Origin.** This entry was concretised by portal-sync hand-rolling its own `NodeService` / `NodeInfo` (a node describing itself). The generic half of that — "a node can describe itself" — is baseline. The rest of portal-sync's record (admission `state: active|revoked|retired`, expiry, policy attributes) is **domain policy** and stays in portal-sync's namespace. The rule that falls out: **Aster ships the introspection mechanism + a generic identity record; it does not absorb any one consumer's policy schema.** Consumers layer their own service on top (ideally embedding `NodeIdentity` rather than re-declaring `node_id`).

**`NodeIdentity` (SHIPPED — wire `aster/NodeIdentity`, `aster/src/rpc/baseline.rs`).** Mirrors the node's real substrate — Aster's `AttributeStore` is `HashMap<String, HashMap<String, String>>` (`aster/src/rpc/auth.rs:22`), i.e. single-valued name→value:

```
node_id: String        # hex public key (NodeId) — stamped by the server at start; unforgeable by config
node_name: String
tags: Vec<String>
roles: Vec<String>     # hoisted out of attributes — Gate-3 load-bearing (aster.role)
attributes: Vec<Attr>  # everything else; Attr { name: String, value: Option<Value> }
```

Rejected the portal-sync `attr_keys` / `attr_kinds` / `attr_values` parallel-array shape: three index-aligned `Vec<String>` encode a desync bug the type can't prevent, and double-stringly-type the value. A `Vec<Attr>` of a self-contained pair fixes both.

**`aster.ops.NodeInfo` (SHIPPED).** One idempotent method, `describe() → NodeIdentity`. Served **by default** on every `AsterServer` (which also makes a service-less server startable — a bare node is still describable); `builtin_node_info(false)` disables, `node_info_requires(…)` gates it (Gate 3, via a `RequireGuard` service-requirement wrapper), `node_identity(…)` supplies the initial record. The served record is **live**: `AsterServer::node_identity()` returns a clone-shared `NodeIdentityHandle` (AttributeStore pattern) with `get`/`set`/`update` for runtime changes (drain flags, capacity, retagging); `node_id` stays pinned through every mutation. Tests: `aster/tests/rpc_baseline.rs`.

### Shared value type — `aster/Value` (SHIPPED)

`Value` is referenced across the surface (e.g. `map<string, Value>` in `LogRecord` below, and `Attr` above): one typed primitive union, `Int(i64) | Float(f64) | Bool(bool) | Text(String)` (Int/Float deliberately distinct: JS/TS `number` makes i64-vs-f64 a real cross-binding distinction).

Two shape decisions were forced by the normative spec during implementation:

- **Variants are one-field wrapper messages** (`aster/IntValue` etc.), not bare primitives. ContractIdentity **§11.3.3 v1 admits only message-typed union variants** — `UnionVariantDef` carries only `{name, id, type_ref-of-message}`, with no slot for a primitive; primitive variants "MUST raise in v1". Allowing them is a canonical-bytes change that must land in **all four** bindings' encoders simultaneously (they already model unions identically), so it's a flagged spec-revision follow-up, not something to slip in Rust-side. Ergonomics are recovered with `From` impls + `as_*` accessors — callers never touch the wrappers.
- **No `Null` variant.** §11.3.3 rule 2: `T | nothing` is nullability, not a union — absence is `Option<Value>` at the field (see `Attr.value`). ForyUnion's mandatory `#[fory(default)]` sits on `Text` instead, and its mandatory `#[fory(unknown)] Unknown(fory_core::UnknownCase)` forward-compat variant is a runtime artifact excluded from the contract.

**Two-layer state of the world (updated 2026-07-02, post-ship):**

- **Contract-identity layer models unions cross-binding** — and Rust now *emits* it: `#[derive(AsterType)]` accepts data-carrying enums → `TypeDefKind::Union` + `union_variants`, ids per the ForyUnion case-id scheme (explicit `#[fory(id = N)]`, else 0-based declaration index, `Unknown` excluded). Canonical hash is declaration-order-independent (encoder sorts by id) and pinned by a golden in `aster/tests/rpc_contract.rs`. Java/Python/TS already model `UnionVariantDef` identically; cross-check their encoders against the golden when their union codegen lands.
- **Value codec:** Rust round-trips unions through the payload Fory (`ForyUnion`, fory-core 1.3.0). Python/TS/Java/Kotlin still have no union value codec (Python peels `Optional` only; TS hard-codes `union_variants: []`; Fory version skew: rest of the matrix is on 0.17.0). Rides the existing cross-binding-payload track — exactly as sequenced.

**Build order — where it stands:**
0. ✅ Namespace guard in the macros (see *Namespace reservation*).
1. ✅ `#[derive(AsterType)]` union support (`aster-macros/src/lib.rs::expand_union`), tested incl. golden hash pin.
2. ✅ `aster/Value` + Rust↔Rust Fory round-trip (`aster/tests/rpc_contract.rs::union_value_roundtrips_through_payload_fory`).
3. ✅ `aster/Attr` + `aster/NodeIdentity` + `aster.ops.NodeInfo` default-on in `AsterServerBuilder` (end-to-end tests in `aster/tests/rpc_baseline.rs`).
4. ⏳ Per-binding union **value** codec + codegen (Python/TS/Java/Kotlin) + Fory 0.17→1.x reconciliation — tracked under the cross-binding-payload effort. Also carries the FFI surfaces for `NodeInfo` in each binding.
5. ⏳ (Flagged spec revision, optional) Direct primitive union variants — §11.3.3 change to `UnionVariantDef` canonical form, all four encoders at once; would let `Value` drop the wrapper messages in a v2.

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
                            # (see workload_identity.md §"Pluggable
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
aster.ops.NodeInfo          # ✅ SHIPPED (Rust, default-on; aster/src/rpc/baseline.rs)
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

> Every Aster node exposes the same baseline surface. A control panel, a CLI, a monitor knows it can dial any node and call the same ops RPCs and get a predictable response. They ship with the runtime, on by default, namespace-reserved in `aster.*`, and gated by scopes. (As shipped: the application *can* disable or auth-gate a baseline service via the builder — default-on with explicit opt-out replaced the original "never disable" stance; what stays non-negotiable is the reserved namespace and the uniform shape when the service is present. Whether some services become truly mandatory is part of the scope-model session.)
>
> The "connect and see" surface (Logs, Metrics, Traces) takes a filter parameter so the operator can drill down without dragging back the whole stream. The pull RPCs and the gossip-based push substrate share a single in-process buffer in core, so an operator gets immediacy when they want it and durability when they want that.
>
> Everything in this doc is an idea, not a spec. The shipping surface will be smaller. The point of writing the full vision is to make the smaller version a deliberate cut, not a default.
