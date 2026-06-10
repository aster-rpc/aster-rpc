# Decentralized Log Distribution — Aster as the Substrate

**Status:** Working idea
**Date:** 2026-05-06
**Companion docs:**
- [observability-in-core.md](observability-in-core.md) — OTel SDK, metrics, traces, logger trace-context hooks. This doc extends the logs section's boundary.
- [aster-tunneld-linux.md](aster-tunneld-linux.md) — sibling pattern (Aster as the substrate replacing a category of infra)
- [identity-worked-example.md](retired.identity-worked-example.md) — tenant model that frames per-tenant log isolation

---

## Reconciling with `observability-in-core.md`

The Logs section of the observability doc said "Aster doesn't ship a logs pipeline." That position needs nuance, not reversal:

> **Aster doesn't ship a logging framework.** Users keep SLF4J / `logging` / `pino`. Aster ships hooks for trace context.
>
> **Aster *can* ship a log distribution substrate**, because logging is a bigger concern than just the framework — it's about getting logs to the right place, and for decentralized / P2P / edge / customer-owned deployments there is no "right place" by default.

The two stances are compatible. The earlier framing was the right answer for centralized SaaS-on-k8s deployments where fluent-bit + Loki / Datadog already exist. This doc is about the deployments that *don't* have that — and where Aster's existing primitives (in-process runtime, iroh-blobs, gossip, contract identity) make a uniquely good substrate available.

| Deployment shape | Right log path |
|---|---|
| SaaS on k8s, with Datadog / Loki / Splunk | App → stdout → fluent-bit / vector → vendor backend (Aster stays out of the way) |
| Customer-owned VM, no node agent | App → AsterLogAppender → iroh-blobs + gossip → operator-side archiver (this doc) |
| User's laptop / desktop app | App → AsterLogAppender → iroh-blobs + gossip → vendor's control plane on demand (this doc) |
| Edge / IoT mesh | App → AsterLogAppender → peer mesh → opportunistic upload (this doc) |
| Air-gapped enterprise | App → AsterLogAppender → in-perimeter Aster archiver → in-perimeter S3 (this doc) |

Both paths can coexist in the same binary — the user can wire SLF4J to write to *both* stdout (for the cluster case) and the Aster appender (for the decentralized case). They are not mutually exclusive.

---

## What's driving this

Two pressures, both real:

1. **Operational complexity / cost of centralized log stacks.** Running ELK or Loki yourself is a multi-engineer job; running Datadog or Splunk costs $1.60–$3 per GB ingested. For a Portal-sized customer at 100 GB/day this is $5k–$10k/month before retention. Operators are looking for ways out, especially when most of their query patterns are bounded ("find errors in the last hour for tenant X") rather than open-ended ("anomaly detection across all logs").

2. **Decentralized deployment story.** Aster's reason to exist is that not every system fits the cluster-owned model. A Portal VM running on a customer's bare EC2 instance, an Aster CLI on someone's laptop, an edge device on someone's network, a P2P mesh with no central operator — none of these have a "node agent" to lean on. If logs from these nodes can only be obtained by the customer setting up their own log pipeline first, the operator-experience story has a hole.

The opinionated take: Aster's existing primitives compose into a substrate that solves both pressures *without* turning Aster into a logging product. The application keeps its existing logger; Aster handles "get this log record from the application to the place it needs to go," using the Aster mesh as the distribution channel.

---

## Thesis in one sentence

The Aster runtime, already in every application's process for RPC, becomes the in-process log distribution substrate — buffering log records, batching them as Parquet blobs, publishing via iroh-blobs + gossip — and any number of subscribers (control-plane live-tail UI, S3 archiver, SIEM bridge, peer node) consume the stream without the application owning a log pipeline or running a sidecar.

---

## Architecture

```
┌─ Application process ────────────────────────────────────────┐
│                                                              │
│  log.info("workspace launched", workspace_id=ws.id)          │
│       │                                                      │
│       │ via SLF4J / logging / pino (UNCHANGED user code)     │
│       ▼                                                      │
│  AsterLogAppender                                            │
│   (one line in Logback / logging.config / pino options)      │
│       │                                                      │
│       │ trace_id, span_id, baggage already in MDC / contextvars
│       │ (from the existing observability hooks)              │
│       │                                                      │
│       ▼                                                      │
│  Local buffer (SlateDB / rotating Parquet)                   │
│   - durable; survives crashes; spills when offline           │
│       │                                                      │
│       ▼                                                      │
│  ┌─ Aster runtime (already there for RPC) ────────────────┐ │
│  │  Background tokio task:                                 │ │
│  │   - every N sec OR N MB:                                │ │
│  │     1. Flush buffer → encrypted Parquet blob            │ │
│  │     2. iroh-blobs.add(blob) → ticket                    │ │
│  │     3. gossip.broadcast(<topic>, ticket + small manifest)│ │
│  │   - retries, backpressure, local spill                  │ │
│  └─────────────────────────────────────────────────────────┘ │
└──────────────────────────────┼───────────────────────────────┘
                               │
                               │ iroh QUIC + iroh-gossip
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
       Control plane      S3 archiver      Compliance / SIEM bridge
      (live-tail UI)   (durable cold      (push to Splunk, Sentinel,
                       store, dedup,       regulated WORM stores, ...)
                       compaction)
```

Three things to notice:

- **The application's logger calls don't change.** SLF4J / `logging` / `pino` work as they always did. The Aster appender is one config line.
- **No sidecar process.** The publisher runs as background tasks on the Aster runtime that's already in the process for RPC. No second binary, no socket, no `/var/lib` mount, no separate health story.
- **Subscribers are decoupled.** The publisher doesn't know who's listening. New subscriber types (a regulator's archive, a peer node's local copy, a developer's debug tail) can come and go without touching the publisher.

---

## Publisher: in-process, no sidecar

This is the architectural move that makes everything else viable. Compare to the alternatives:

| Approach | Process | Operational story |
|---|---|---|
| Sidecar agent (fluent-bit, vector, promtail) | Separate process | Install + run + monitor + upgrade + secret-manage on every node |
| In-app SDK (vendor-shipped) | In application process | Per-language vendor lib; per-vendor lock-in; pay-per-byte |
| **AsterLogAppender on the Aster runtime** | **In application process, on existing runtime** | **Zero extra processes; rides existing Aster operational story** |

Because the Aster runtime is already running, the log batcher, blob writer, gossip publisher, retry logic, and backpressure handling are *background tasks on the existing tokio runtime.* This is the unique property — nobody else can do "no sidecar" without forcing a vendor SDK, because nobody else has an in-process P2P mesh runtime as a baseline.

### Local buffer choice

The local buffer is the durable bridge between "log line emitted" and "blob published." Requirements:

- Survives application crash (durable to local disk).
- Bounded growth (rotates / spills when too large; configurable cap).
- Cheap inserts (sub-microsecond overhead per log record).
- Cheap rotation (no copy of the whole buffer to publish a batch).

Two viable backends:

1. **SlateDB** (Rust, GA in 2025). LSM over local files with optional direct S3 mode. Designed for exactly this "embedded write path with object-storage-aware cold tier." Good fit; well-maintained; same language as core.
2. **Rotating Parquet files.** Simpler. Application writes records into a row group; on flush we close the file and publish. No LSM. Works fine for log shapes where flush cadence is consistent.

Default to (2) for v1 (less to get wrong), evaluate (1) when query workloads from local buffers become a thing (e.g., on-device debugging UI).

---

## Logger integration — the standard appender pattern

Every modern logging framework has a "ship a record somewhere" extension point. The Aster integration uses each framework's idiomatic mechanism — **not a new logger.**

| Logger | Extension point | Aster-side LOC |
|---|---|---|
| Logback | `Appender` extending `UnsynchronizedAppenderBase` | ~80 |
| Log4j2 | `@Plugin` annotated `Appender` | ~80 |
| Python stdlib `logging` | `logging.Handler` subclass | ~50 |
| `structlog` | processor that also writes to sink | ~30 |
| `loguru` | `logger.add(sink_fn)` | ~20 |
| `pino` | transport (worker thread or inline) | ~60 |
| `winston` | `Transport` subclass | ~60 |
| .NET (future) | `ILoggerProvider` | ~100 |

User wiring example, Logback (Java):

```xml
<configuration>
  <appender name="ASTER" class="site.aster.logging.AsterLogAppender">
    <topic>portal.tenant-T.logs</topic>
    <batchMaxBytes>4194304</batchMaxBytes>     <!-- 4 MB -->
    <batchMaxIntervalMs>5000</batchMaxIntervalMs>
  </appender>
  <appender name="STDOUT" class="ch.qos.logback.core.ConsoleAppender">
    <encoder class="net.logstash.logback.encoder.LogstashEncoder"/>
  </appender>
  <root level="INFO">
    <appender-ref ref="STDOUT"/>      <!-- still goes to stdout for k8s -->
    <appender-ref ref="ASTER"/>       <!-- AND distributed via Aster -->
  </root>
</configuration>
```

Python:

```python
import logging
from aster.logging import AsterLogHandler

handler = AsterLogHandler(
    topic="portal.tenant-T.logs",
    batch_max_bytes=4 * 1024 * 1024,
    batch_max_interval_s=5,
)
logging.getLogger().addHandler(handler)
```

pino (TypeScript):

```typescript
import pino from 'pino';
import { asterTransport } from '@aster/aster';

const log = pino({}, pino.multistream([
  { stream: process.stdout },                    // also keep stdout
  { stream: asterTransport({ topic: 'portal.tenant-T.logs' }) },
]));
```

The application's own log calls — `log.info(...)` — are unchanged. The user opts into Aster distribution by adding one appender; opts back out by removing it. Same as wiring up Splunk or Datadog appenders today.

### Trace context comes for free

Because the trace-context hooks from `observability-in-core.md` already populate MDC / contextvars / AsyncLocalStorage with `trace_id`, `span_id`, and baggage entries, **every record the AsterLogAppender ships already includes those fields.** No extra wiring. Click metric → trace → log lines for that specific request works end-to-end.

---

## Substrate: iroh-blobs + iroh-gossip

The transport choice is what makes this Aster's idea and not just "buffer + S3":

- **iroh-blobs** is content-addressed P2P blob transfer. A blob's hash is its identity. Publishing means computing the hash and announcing; consumption means asking any peer who has the hash to send it. Dedup is automatic across publishers, retransmits are free, and a blob in flight on the network is identical to one at rest.

- **iroh-gossip** is logarithmic-fanout pub-sub on the same mesh. Topic membership is dynamic; publishers don't know subscribers; subscribers join and leave without coordination.

A log batch becomes:

```
{
  "blob": <iroh-blobs ticket>,    // pull this to get the Parquet bytes
  "node_id": <publisher's iroh endpoint id>,
  "tenant_id": "T",
  "service": "portal-cp",
  "first_ts": 1748160000.123,
  "last_ts":  1748160005.876,
  "row_count": 8421,
  "schema_id": "portal.v1",
  "sig": <signed by node's deployment-root admission key>
}
```

…broadcast on a topic like `aster.logs.v1.tenant-T`. ~300 bytes of metadata; the actual log Parquet (1–10 MB compressed) lives in iroh-blobs and is fetched only by subscribers who want it.

### Why this beats "publish full bytes through gossip"

Gossip is for *announcements*, not bulk transport. Putting log bytes in gossip floods every peer in the topic. Putting only the hash in gossip + fetching the bytes via iroh-blobs means:

- Peers who don't care about a given batch never download it.
- Peers who want the same batch dedup automatically (hash equality).
- Late-arriving subscribers can backfill by walking the gossip history and pulling blobs they're interested in.

### Topic naming convention (proposed)

```
aster.logs.v1.<tenant_id>                   # all logs for a tenant
aster.logs.v1.<tenant_id>.<service>         # one service
aster.logs.v1.<tenant_id>.<service>.<sev>   # by severity (rare)
```

Subscribers pick the granularity that matches their bandwidth / interest. The control panel might subscribe to `aster.logs.v1.tenant-T`; an SRE doing live debug might subscribe to `aster.logs.v1.tenant-T.portal-cp` for one service.

---

## Subscribers — the open-ended consumer side

The publisher doesn't know who is listening. Anyone in the mesh with the right admission token can subscribe to a topic and start consuming.

### Control plane live-tail UI

The most user-visible subscriber. The operator opens the control panel for tenant T:

1. Control plane subscribes to `aster.logs.v1.tenant-T`.
2. For each gossip announcement, fetches the blob via iroh-blobs (P2P, fast — peers help each other).
3. Decodes Parquet, streams rows over WebTransport to the browser.
4. Browser renders, with client-side filtering and search.

The first time a customer launches a Portal VM they should see logs streaming in the dashboard within seconds, with **no additional setup.** That is the Day-0 demo moment this whole architecture is designed to enable.

### S3 archiver

The durable cold-storage subscriber. Subscribes to all tenant topics (or a partition of them). Receives every blob, writes to `s3://aster-logs/<tenant>/<date>/<blob-hash>.parquet` with content-addressed paths, deduplicates on hash, and prunes its local copy after upload-confirmed.

This subscriber is the only one that needs significant disk; everyone else is opportunistic.

### SIEM / compliance bridge

For regulated customers, a subscriber that takes the same stream and pushes to Splunk, Microsoft Sentinel, AWS Security Lake, or a write-once-read-many archive. Same code path; different egress.

### Peer node

In a P2P mesh (no central operator), peers can subscribe to each other's topics. Useful for resilience: if node A's S3 connection is down, node B keeps a copy of A's recent logs in iroh-blobs and can serve them to a subscriber later. Logs become as durable as the mesh.

### Developer ad-hoc tail

`aster log tail --tenant T --service portal-cp` is just a CLI subscriber that streams to stdout. No infrastructure needed, just an Aster identity authorised on the topic.

---

## Scalability analysis

### Per-publisher cost

Typical service emitting moderate logs: ~10 MB/min of compressed Parquet.

- Local buffer write: sub-µs per record (memory + WAL append).
- Periodic flush: ~10 MB Parquet write, takes ~50 ms on a slow disk.
- iroh-blobs add: hash computation + local store insert; sub-second for 10 MB.
- Gossip announce: ~300 bytes; one round of fanout; milliseconds.

Net publisher cost: well under 1% of one core for a service emitting 10 MB/min. Could publish at 100 MB/min before this is the bottleneck.

### Gossip fanout

iroh-gossip is logarithmic. 10k subscribers on a topic costs each publisher ~log(10k) ≈ 14 sends per announcement. The announcement itself is small (~300 bytes), so even at 1 batch/sec the gossip overhead per publisher is ~4 KB/s — negligible.

### Per-subscriber cost

This is where a subscriber must think. Three regimes:

| Subscriber profile | Topics subscribed | Bandwidth bound |
|---|---|---|
| Live-tail UI for one tenant | 1 | ~10 MB/min × N services in tenant |
| All-tenants archiver | All | Sum across all publishers; could be GB/min for a large fleet |
| SIEM bridge for one regulated tenant | 1 | Same as live-tail |
| Peer with opportunistic interest | Variable | Self-limiting (peer stops fetching when local cache is full) |

Filter at the subscriber, not the publisher. A subscriber that wants severity≥WARN only still subscribes to the same topic but drops blobs whose manifest indicates only INFO-level rows (manifest carries severity range).

### Blob count over time

Steady-state estimate: 10k nodes × 1 blob/min × 365 days ≈ 5.2 billion blobs/year.

iroh-blobs handles individual blobs fine, but the metadata index gets large. Mitigation: **compaction** — a background process rolls up small blobs into larger time-windowed ones (hourly → daily → weekly) and re-announces. Same Parquet rows, fewer files, smaller index. Detail deferred to the next session (see "Open / next sessions").

### Subscriber-side storage

The S3 archiver is the only subscriber that wants every byte. Local copy is transient — write to S3, prune local. Live-tail subscribers keep a rolling window (last hour, last day). Peer subscribers cap their local cache and let LRU evict.

---

## Auth and encryption — sketch (full design later)

Two layers:

1. **Topic membership.** Joining a gossip topic requires presenting an Aster admission token / rcan that grants `subscribe` on `aster.logs.v1.tenant-T`. The trust spec already has the primitives (`tenant_id`-scoped tokens via the pluggable enrollment-proof framework). Unauthorized peers are rejected at the gossip-layer handshake.

2. **Payload encryption.** The Parquet blob bytes are encrypted with a tenant-derived key before `iroh-blobs.add`. Even an unauthorised peer who somehow pulled the bytes (e.g., from a public DHT) gets ciphertext. Key derivation rides the trust anchor → deployment-root chain. Key rotation is a real concern and gets its own session (see below).

These are sketches. The detailed design — specifically key derivation, rotation cadence, recovery-from-key-compromise, cross-tenant isolation guarantees — needs the dedicated session flagged below.

---

## What this enables

- **One-line operator experience.** The customer launches a Portal VM, opens the control panel, sees logs streaming. No log-shipping config, no node agent, no port to open.
- **Decentralized deployments stop being log-blind.** A user running an Aster app on their laptop or an air-gapped network gets the same operator-side observability story as a SaaS deployment.
- **Cost story that doesn't compound with vendor pricing.** S3 storage at $0.023/GB/month + occasional query compute beats $1.60/GB ingestion every time, at any volume.
- **Genuine P2P log durability.** When archive uplinks are down, peers hold each other's logs. When they reconnect, archives drain. The mesh is the buffer.
- **No vendor lock-in.** Parquet is universally readable. The S3 bucket can be queried by DuckDB, Trino, Athena, ClickHouse, Polars, pandas — whatever the customer's analytics stack already uses.

---

## What this is NOT

- **Not a logging framework.** Users keep SLF4J, `logging`, `pino`. AsterLogAppender is one config line.
- **Not a logs-only product.** This rides the same iroh-blobs + gossip substrate that distributes blobs and announcements for everything else in Aster. Logs are the first big consumer; metrics-as-events and traces could ride the same channel.
- **Not a search engine.** Day-1 query is "load Parquet, scan with predicate pushdown via DuckDB." This is good for bounded queries (one tenant, one day, one service). Open-ended full-text search across years of logs is a separate product (Quickwit / OpenObserve / Splunk own that). If a customer needs that, they can run one of those engines pointed at the same S3 bucket.
- **Not a replacement for Datadog.** For customers happy with Datadog (or Loki, or whatever), Aster stays out of the way — they keep stdout + node-agent. This is for the deployments where Datadog doesn't fit.
- **Not a sidecar.** That's the whole point.

---

## Open / next sessions

The user explicitly flagged these as worth a dedicated opinionated design session. Listed here as the explicit roadmap for follow-up; not solved in this doc.

### 1. Compaction

Rollup of small blobs into larger time-windowed ones over time (hourly → daily → weekly → monthly). Who runs the compactor (the operator's archiver; one per tenant; leader-elected?), how compaction interacts with already-pulled blobs in subscriber caches, how the compaction event is announced (gossip a "the following 60 hourly blobs are now superseded by this one daily blob" message), and what consistency guarantees we offer during the rollover window.

### 2. Retention policies

Per-tenant retention (90 days hot, 1 year cold, delete after 7 years for GDPR). Where the retention rules live (tenant config in the control plane), how the archiver enforces them (S3 lifecycle rules + a sweeper for in-mesh blobs), and how a deletion request propagates to peers who may still hold copies.

### 3. Encryption key rotation

Tenant key derivation, key versioning so old blobs stay decryptable after rotation, the rotation cadence and trigger (calendar / breach-response / customer-initiated), and the operator playbook for "we suspect a key was compromised."

### 4. Schema evolution

Log schemas change over time (new fields added, old fields renamed, semantics shift). Parquet handles missing columns; renames hurt. Options: schema versions in the manifest (`schema_id: "portal.v3"`), schema registry as an Aster service, or just lean on column-mapping conventions and document what breaks.

### 5. Query tooling

DuckDB-against-S3 works for a developer at a terminal. Operators want something more structured: a control-plane query API that scopes by tenant + admission, returns paginated rows, supports basic filters and aggregations. Could be a thin layer on top of `duckdb_engine`, or a more opinionated query DSL.

### 6. Ingestion guarantees

What the operator can tell their customer about log durability:
- "At-least-once" by default — local buffer survives crash, archiver dedups by hash.
- Latency from emit to "visible in archive" — typical ~30s, worst-case during outages.
- What happens during a multi-hour S3 outage (local buffer fills; backpressure to logger? drop oldest? spill to disk?).
- "Exactly-once" semantics if regulated customers need them (probably via the archiver doing manifest-tracked transactional writes).

### 7. Observability of the log substrate itself

Meta-question: how do operators see whether the log distribution is healthy? Metrics on publisher local-buffer depth, gossip announce success rate, archiver lag, blob-fetch failures. These come back to the metrics in `observability-in-core.md` — the substrate emits them via the same OTel pipeline.

### 8. SDK ergonomics for ad-hoc subscribers

A `aster log` CLI (tail, search-recent, archive-manage), a `aster.logs.subscribe()` library API for users building custom dashboards, and a documented "build your own subscriber" path for the SIEM / regulator integration cases.

The pull-side counterpart of this — direct `aster.ops.Logs.Tail()` from one node — is now sketched in [aster-baseline-services.md](aster-baseline-services.md). The gossip path (this doc) and the pull RPC (that doc) share the same in-core local buffer; together they cover both fleet-wide always-on observability and "drop into one node right now" debugging.

### 9. Coexistence with stdout-based pipelines

Document the multi-appender pattern explicitly: same app can write to stdout (cluster picks up), to AsterLogAppender (mesh distribution), to a file (regulatory archive), all simultaneously without the application caring. Show the recipe for each binding.

### 10. Compaction-vs-mutation immutability invariants

Once a blob is published, it's immutable (content-addressed). Compaction creates new blobs; old ones can be GC'd after subscribers acknowledge. But the immutability guarantee should be explicit so subscribers can cache aggressively without staleness concerns.

---

## Mental model

> The Aster runtime is already in the application's process. It's already running gossip and iroh-blobs. The application's logger already has an appender extension point. Connecting these three things gives you a sidecar-free, vendor-free, P2P-durable log distribution substrate that costs the operator nothing extra to run, scales with the mesh, and shows up in the control panel automatically.
>
> What's left is the opinionated bits — compaction, retention, encryption key rotation, schema evolution, query tooling, ingestion guarantees — which are exactly the design surfaces where a small team can build a coherent product instead of fighting a vendor.
