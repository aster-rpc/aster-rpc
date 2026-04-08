# Aster Node Observability — Design Spec

**Status:** Draft  
**Date:** 2026-04-08  
**Scope:** Expose transport, RPC, and application-level metrics through the CLI shell (VFS + watch), non-interactive CLI automation, and producer-authored custom metrics API.

---

## Table of Contents

1. [Motivation](#1-motivation)
2. [The SRE Story](#2-the-sre-story)
3. [Architecture Overview](#3-architecture-overview)
4. [Phase 1: VFS Observability Nodes](#4-phase-1-vfs-observability-nodes)
5. [Phase 2: Watch — Live Dashboards](#5-phase-2-watch--live-dashboards)
6. [Phase 3: CLI Automation (Non-Interactive)](#6-phase-3-cli-automation-non-interactive)
7. [Phase 4: Producer Custom Metrics API](#7-phase-4-producer-custom-metrics-api)
8. [Phase 5: Custom Metrics in the Shell](#8-phase-5-custom-metrics-in-the-shell)
9. [gRPC Familiarity Bridge](#9-grpc-familiarity-bridge)
10. [Prometheus & OTel Integration](#10-prometheus--otel-integration)
11. [Exit Criteria](#11-exit-criteria)
12. [Dependency Map](#12-dependency-map)

---

## 1. Motivation

The Aster CLI shell is developer-facing today: services, blobs, docs, gossip. An SRE who shells into a running producer node to diagnose a 3am page sees *what* the node offers but nothing about *how it's doing*. No peers, no bandwidth, no connection health, no packet loss, no application metrics.

The irony: the data exists. Transport metrics are collected in Rust, bound to Python via PyO3, and exposed as `transport_metrics()`, `remote_info_list()`, and per-connection stats. The `MetricsInterceptor` tracks RED metrics. But none of it is reachable from the shell, and there is no API for producers to expose their own application-level gauges and counters.

Three problems to solve:

1. **Discoverability** — Transport and RPC metrics should be navigable via the same VFS metaphor (`cd`, `ls`, `cat`) that already works for services and blobs. No new commands to memorize.

2. **Automation** — SREs script things. Every observable surface must be reachable non-interactively: `aster net peers <addr>`, `aster metrics transport <addr> --format=prometheus`. Pipe-friendly. JSON by default.

3. **Application metrics** — Producers need a first-class API to publish business-level metrics (queue depth, processing lag, cache hit rate) alongside the framework's built-in metrics. These must appear in the same VFS tree and be exportable through the same automation paths.

A secondary concern: **gRPC muscle memory**. Aster is new. The SRE who learned `grpcurl`, `grpc_health_v1`, and Envoy stats shouldn't have to start from scratch. We should map familiar concepts to their Aster equivalents and provide a bridging guide, not force people to discover everything by exploration.

---

## 2. The SRE Story

> *This section is the narrative specification. Implementation details follow in later phases.*

### 2.1 The Page

3am. Gossip messages are lagging between producer nodes in `us-east-1`. You get paged. You know the node ID. You shell in:

```
$ aster shell n4bma...qk7e --rcan ops.rcan
┌──────────────────────────────────────────┐
│ Connected to prod-ingest-03              │
│ 4 services · 12 blobs · 3 peers         │
│ uptime 14d 6h · relay us-east-1         │
└──────────────────────────────────────────┘
```

The welcome banner already tells you more than it used to: peer count, uptime, relay region.

### 2.2 Orient — `ls /`

```
prod-ingest-03:/$ ls
blobs/       12 blobs (48.2 MB)
services/     4 services
docs/         1 document
gossip/       2 topics
net/          3 peers, 2 direct / 1 relay
metrics/      transport · rpc · app
```

Two new top-level VFS nodes: **`/net`** and **`/metrics`**. They're navigable, not commands. You explore them the same way you explore services. Tab completion works. `--json` piping works. Everything you already know about the shell applies.

### 2.3 Check Peers — `cd /net`

```
prod-ingest-03:/net$ ls
peers/         3 connected
connections/   5 active
relay          us-east-1.relay.iroh.net
```

```
prod-ingest-03:/net$ ls peers/
NODE ID          TYPE      RTT     SENT     RECV     LAST SEEN
n4xz..q3f8      direct    2ms     1.2 GB   890 MB   12s ago
n4ab..k2e1      direct    4ms     620 MB   1.1 GB   3s ago
n4ff..p9w2      relay     142ms   89 MB    12 MB    45s ago
```

That relay peer jumps out. Drill in:

```
prod-ingest-03:/net$ cd peers/n4ff..p9w2
prod-ingest-03:/net/peers/n4ff..p9w2$ cat .
Node ID:         n4ff...p9w2
Connection:      relay (us-east-1.relay.iroh.net)
RTT:             142ms (min: 38ms, max: 420ms)
Bytes sent:      89 MB
Bytes received:  12 MB
Last handshake:  2m ago
Holepunch:       3 attempts, 0 succeeded
Connections:     2 active
```

Stuck on relay, holepunch failing. That explains the elevated latency.

### 2.4 Check Connections

```
prod-ingest-03:/net$ ls connections/
CONN ID    PEER           ALPN            RTT    CWND    LOST   CONG
c-001      n4xz..q3f8    /aster/1        2ms    64KB    0      0
c-002      n4xz..q3f8    /iroh-bytes/4   2ms    128KB   0      0
c-003      n4ab..k2e1    /aster/1        4ms    64KB    0      0
c-004      n4ff..p9w2    /aster/1        142ms  12KB    847    23
c-005      n4ff..p9w2    /iroh-bytes/4   139ms  8KB     1204   41
```

Packet loss and congestion events on the relay peer. The congestion window has collapsed to 8–12KB. Now you know *why* gossip is lagging — the relay path to `n4ff..p9w2` is saturated.

### 2.5 Check Throughput — `cd /metrics`

```
prod-ingest-03:/metrics$ ls
transport      bytes, connections, paths, holepunch
rpc            RED metrics (rate, errors, duration)
app/           2 custom metric groups
```

```
prod-ingest-03:/metrics$ cat transport
METRIC                    VALUE          RATE
Bytes sent (IPv4)         1.8 GB         12.4 MB/s
Bytes sent (IPv6)         0 B            —
Bytes sent (relay)        89 MB          0.2 MB/s
Bytes recv (IPv4)         1.9 GB         11.8 MB/s
Bytes recv (IPv6)         0 B            —
Bytes recv (relay)        12 MB          0.04 MB/s
Datagrams received        4,891,203      ~340/s
Connections opened        847
Connections closed        842
Active connections        5
Direct paths              2
Relay paths               1
Holepunch attempts        3
Relay home changes        0
```

```
prod-ingest-03:/metrics$ cat rpc
RPC calls started:     148,203
RPC calls succeeded:   147,891
RPC calls failed:      312
In-flight:             0
Error rate:            0.21%

BY SERVICE                STARTED    FAILED    p50     p99
IngestPipeline            98,412     201       2ms     45ms
HealthCheck               49,100     0         <1ms    1ms
ConfigSync                691        111       12ms    890ms
```

ConfigSync has a 16% error rate. Something to flag but not the gossip problem.

### 2.6 Check Application Metrics — `cd /metrics/app`

The producer author published custom metrics via the Aster metrics API:

```
prod-ingest-03:/metrics/app$ ls
ingest/        queue depth, batch lag, throughput
cache/         hit rate, evictions, size
```

```
prod-ingest-03:/metrics/app$ cat ingest
METRIC                    TYPE       VALUE
queue_depth               gauge      4,291
batch_lag_ms              gauge      892
events_processed          counter    12,847,291
events_per_sec            rate       ~3,400/s
last_flush_ms             gauge      45
failed_batches            counter    12
```

Queue depth at 4,291 and batch lag at 892ms — the relay bottleneck is causing backpressure in the application. The story connects: relay peer degraded → congestion → backpressure → gossip lag.

### 2.7 Watch It Live

The `watch` command works anywhere in the VFS:

```
prod-ingest-03:/metrics$ watch transport
┌─ Transport Metrics (every 2s) ──────────── Ctrl+C to stop ─┐
│                                                              │
│  ▸ Throughput    TX: 12.4 MB/s    RX: 11.8 MB/s            │
│  ▸ Direct        2 paths          avg RTT: 3ms              │
│  ▸ Relay         1 path           avg RTT: 142ms            │
│  ▸ Connections   5 active         847 total opened           │
│  ▸ Loss          pkts: 2,051      cong events: 64           │
│                                                              │
│  ── TX bytes/s (30s) ──                                     │
│  ▁▂▃▅▇▇▆▅▅▆▇▇▅▃▂▂▃▅▆▇▇▆▅▄▃▃▄▅                             │
└──────────────────────────────────────────────────────────────┘
```

```
prod-ingest-03:/net$ watch peers/
┌─ Peers (every 2s) ──────────────────────── Ctrl+C to stop ─┐
│ NODE ID      TYPE    RTT     TX/s       RX/s       STATUS   │
│ n4xz..q3f8   direct  2ms    8.1 MB/s   7.2 MB/s   healthy │
│ n4ab..k2e1   direct  4ms    4.3 MB/s   4.6 MB/s   healthy │
│ n4ff..p9w2   relay   142ms  0.2 MB/s   0.04 MB/s  degraded │
└──────────────────────────────────────────────────────────────┘
```

```
prod-ingest-03:/metrics/app$ watch ingest
┌─ ingest (every 2s) ─────────────────────── Ctrl+C to stop ─┐
│  queue_depth     4,291 → 4,103 → 3,980 → 3,812   ▼ drain  │
│  batch_lag_ms    892 → 845 → 801 → 790             ▼ impvg  │
│  events/s        ~3,400                             — steady │
└──────────────────────────────────────────────────────────────┘
```

### 2.8 Export & Automate — No Shell Needed

Everything reachable in the shell is reachable non-interactively:

```bash
# Snapshot, JSON (default for non-interactive)
$ aster net peers n4bma...qk7e --rcan ops.rcan
[{"node_id":"n4xz..q3f8","type":"direct","rtt_ms":2,...}, ...]

# Prometheus text format for scraping
$ aster metrics transport n4bma...qk7e --format=prometheus
# HELP iroh_send_ipv4 Bytes sent via IPv4
# TYPE iroh_send_ipv4 counter
iroh_send_ipv4 1932735488
...

# Application metrics
$ aster metrics app n4bma...qk7e
{"ingest":{"queue_depth":4291,"batch_lag_ms":892,...},"cache":{...}}

# Single metric value for alerting scripts
$ aster metrics app n4bma...qk7e --path ingest.queue_depth
4291

# Pipe into jq, push to Datadog, whatever
$ aster metrics transport n4bma...qk7e | jq '.bytes_sent_ipv4'
```

### 2.9 The Resolution

You've diagnosed the problem without leaving the terminal:

1. **`ls /net/peers`** — spotted the relay-only peer with high RTT
2. **`ls /net/connections`** — confirmed packet loss and congestion on that path
3. **`cat /metrics/transport`** — relay bandwidth negligible vs direct
4. **`cat /metrics/app/ingest`** — backpressure confirmed in application metrics
5. **Root cause:** Holepunch to `n4ff..p9w2` is failing, forcing relay, which can't sustain the gossip volume

You file the ticket, escalate to networking, and go back to sleep. Total time: 4 minutes.

---

## 3. Architecture Overview

```
┌────────────────────────────────────────────────────────────────────┐
│                        CLI / Shell                                 │
│                                                                    │
│  ┌──────────────────────────┐   ┌────────────────────────────┐    │
│  │  Shell (interactive)     │   │  CLI (non-interactive)     │    │
│  │  VFS: /net, /metrics     │   │  aster net peers <addr>    │    │
│  │  watch: live refresh     │   │  aster metrics app <addr>  │    │
│  │  cat: structured display │   │  --format=json|prometheus  │    │
│  └───────────┬──────────────┘   └──────────┬─────────────────┘    │
│              │                              │                      │
│              └──────────┬───────────────────┘                      │
│                         ▼                                          │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │                  Metrics Surface Layer                       │  │
│  │  ObservabilityProvider interface                              │  │
│  │  ├── TransportMetricsProvider  (wraps net_client)            │  │
│  │  ├── RpcMetricsProvider        (wraps MetricsInterceptor)    │  │
│  │  └── AppMetricsProvider        (wraps producer-authored)     │  │
│  └──────────────────────────────────────────────────────────────┘  │
│                         ▼                                          │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │                  Data Sources                                │  │
│  │                                                               │  │
│  │  Transport (PyO3)         RPC (Python)       App (Python)    │  │
│  │  ├ transport_metrics()    ├ MetricsIntcptr   ├ MetricsStore  │  │
│  │  ├ remote_info_list()     │   .snapshot()    │   .gauges     │  │
│  │  ├ remote_info(id)        └ per-service      │   .counters   │  │
│  │  └ connection stats         breakdown        │   .histograms │  │
│  │    (rtt, cwnd, loss)                         └ producer API  │  │
│  └──────────────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────────────┘
```

**Key principle:** A single `ObservabilityProvider` interface serves both the interactive shell and the non-interactive CLI. The shell wraps it in VFS nodes and Rich tables. The CLI wraps it in JSON and Prometheus formatters. Same data, two renderings.

---

## 4. Phase 1: VFS Observability Nodes

**Goal:** Add `/net` and `/metrics` to the VFS tree. Navigable with `cd`, `ls`, `cat`.

### 4.1 New NodeKind Values

```python
class NodeKind(Enum):
    # ... existing ...
    NET = "net"                  # /net
    PEERS = "peers"              # /net/peers
    PEER = "peer"                # /net/peers/<node_id>
    CONNECTIONS = "connections"  # /net/connections
    CONNECTION = "connection"    # /net/connections/<conn_id>
    METRICS = "metrics"          # /metrics
    METRICS_SECTION = "metrics_section"  # /metrics/transport, /metrics/rpc
    METRICS_APP_GROUP = "metrics_app_group"  # /metrics/app/<group>
```

### 4.2 VFS Tree

```
/net/
├── peers/
│   └── <short_node_id>/       (one per remote_info entry)
├── connections/
│   └── <conn_label>/          (one per active QUIC connection)
└── relay                      (leaf — relay URL or "none")

/metrics/
├── transport                  (leaf — global transport counters)
├── rpc                        (leaf — RED metrics from MetricsInterceptor)
└── app/                       (only present if producer publishes custom metrics)
    └── <group_name>/          (one per MetricGroup registered by producer)
```

### 4.3 Populate Functions

```python
async def populate_net(node: VfsNode, connection: PeerConnection) -> None:
    """Populate /net with peers/, connections/, relay."""
    net_client = connection.net_client
    node.add_child("peers", NodeKind.PEERS)
    node.add_child("connections", NodeKind.CONNECTIONS)
    relay = await net_client.relay_url() if hasattr(net_client, 'relay_url') else None
    node.add_child("relay", NodeKind.BLOB, detail=relay or "none")  # leaf

async def populate_peers(node: VfsNode, connection: PeerConnection) -> None:
    """Populate /net/peers/ from remote_info_list()."""
    infos = await connection.net_client.remote_info_list()
    for info in infos:
        short_id = info.node_id[:8] + ".." + info.node_id[-4:]
        child = node.add_child(short_id, NodeKind.PEER)
        child.data = info  # attach RemoteInfo for cat

async def populate_connections(node: VfsNode, connection: PeerConnection) -> None:
    """Populate /net/connections/ from active connection objects."""
    # Requires new API: list active connections (see §4.5)
    pass
```

### 4.4 Display Functions

New functions in `display.py`:

| Function | Renders |
|----------|---------|
| `peer_table(infos)` | Table: Node ID, Type, RTT, Sent, Recv, Last Seen |
| `peer_detail(info)` | Structured view: all fields from RemoteInfo + aggregate |
| `connection_table(conns)` | Table: Conn ID, Peer, ALPN, RTT, CWND, Lost, Cong |
| `transport_metrics_view(metrics, prev)` | Table: Metric, Value, Rate (computed from delta) |
| `rpc_metrics_view(snapshot)` | Summary + per-service breakdown |
| `app_metrics_view(group)` | Table: Metric, Type, Value |

### 4.5 New API Surface Required

The following APIs don't exist yet and are needed:

| API | Layer | Purpose |
|-----|-------|---------|
| `net_client.active_connections()` | PyO3 + core | List active QUIC connections with stats |
| `net_client.relay_url()` | PyO3 + core | Current relay server URL |
| `server.rpc_metrics_snapshot()` | Python | Snapshot from MetricsInterceptor (per-service breakdown) |
| `server.app_metrics_snapshot()` | Python | Snapshot from MetricsStore (see Phase 4) |

### 4.6 Steps

1. Add `NodeKind` values to `vfs.py`
2. Add `/net` and `/metrics` children in `build_root()`
3. Implement `populate_net()`, `populate_peers()`, `populate_connections()`
4. Implement `populate_metrics()`, `populate_metrics_app()`
5. Add display functions to `display.py`
6. Wire `cat` command to render metric/peer/connection nodes
7. Wire `ls` command to show summary lines for new node types
8. Update welcome banner to include peer count, uptime, relay

### 4.7 Exit Criteria

- `cd /net/peers && ls` shows a table of connected peers
- `cd /net/peers/<id> && cat .` shows detailed peer info
- `cd /metrics && cat transport` shows transport metrics
- `cd /metrics && cat rpc` shows RPC RED metrics
- Tab completion works for all new paths
- `--json` mode emits JSON for all new views

---

## 5. Phase 2: Watch — Live Dashboards

**Goal:** Make the existing `watch` command context-aware so it works on any VFS node, not just docs.

### 5.1 Design

`watch` becomes a generic poll-and-refresh command. It takes an optional path argument (defaults to current directory) and refreshes the `cat`/`ls` view at an interval using `rich.live.Live`.

```python
# watch [path] [--interval N]
# Examples:
#   watch transport          (from /metrics)
#   watch /net/peers         (from anywhere)
#   watch /metrics/app/ingest
```

### 5.2 Rate Computation

For counter metrics (bytes sent, calls started, etc.), `watch` mode computes deltas between snapshots and displays rates:

```python
@dataclass
class MetricsDelta:
    current: dict[str, int | float]
    previous: dict[str, int | float]
    interval_s: float

    def rate(self, key: str) -> float:
        return (self.current[key] - self.previous[key]) / self.interval_s
```

Gauges display current value. Counters display value + rate. This distinction is driven by metric type metadata (see Phase 4).

### 5.3 Sparklines

For `watch` on transport metrics and app metrics, maintain a rolling window (30 samples = 60s at 2s interval) and render sparkline characters (`▁▂▃▄▅▆▇█`) for key throughput metrics.

### 5.4 Steps

1. Refactor `watch` command to accept any path, not just `/docs`
2. Implement poll loop with `rich.live.Live`
3. Add `MetricsDelta` utility for rate computation
4. Add sparkline renderer
5. Define which metrics get sparklines vs plain values

### 5.5 Exit Criteria

- `watch /net/peers` refreshes peer table every 2s
- `watch /metrics/transport` shows rates and sparklines
- `watch /metrics/app/<group>` works for custom metrics
- Ctrl+C cleanly exits watch mode

---

## 6. Phase 3: CLI Automation (Non-Interactive)

**Goal:** Every observable surface reachable without entering the shell. JSON by default. Prometheus as an option.

### 6.1 Command Structure

```
aster net <subcommand> <peer-addr> [--rcan PATH]
aster metrics <subcommand> <peer-addr> [--rcan PATH] [--format FORMAT]
```

| Command | Output | Description |
|---------|--------|-------------|
| `aster net peers <addr>` | JSON array of RemoteInfo | List connected peers |
| `aster net peer <addr> <node-id>` | JSON RemoteInfo | Single peer detail |
| `aster net connections <addr>` | JSON array of ConnectionInfo | Active QUIC connections |
| `aster net relay <addr>` | String | Current relay URL |
| `aster metrics transport <addr>` | JSON | Transport metric snapshot |
| `aster metrics rpc <addr>` | JSON | RPC RED metrics snapshot |
| `aster metrics app <addr>` | JSON | All custom metric groups |
| `aster metrics app <addr> --group G` | JSON | Single metric group |
| `aster metrics app <addr> --path G.key` | Scalar | Single metric value |
| `aster metrics all <addr>` | JSON | Combined: transport + rpc + app |
| `aster metrics all <addr> --format=prometheus` | Text | Prometheus exposition format |

### 6.2 Output Formats

| `--format` | Behavior |
|-------------|----------|
| `json` (default) | JSON to stdout, suitable for `jq` |
| `prometheus` | Prometheus text exposition (only for `metrics` subcommands) |
| `table` | Rich table (like shell view, for human use) |
| `value` | Raw scalar, only valid with `--path` |

### 6.3 Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Connection failed |
| 2 | Peer not found / metric path not found |
| 3 | Auth/RCAN error |

### 6.4 Integration with Existing Plugin System

The shell's `plugin.py` already maps shell commands to CLI noun-verb structure. Extend this:

```python
# In commands.py
@register("peers", contexts=["/net", "/net/*"], cli=("net", "peers"))
class PeersCommand(ShellCommand):
    ...
```

The `cli=("net", "peers")` tuple registers the non-interactive subparser automatically. The same `execute()` logic runs in both modes — the display layer switches between Rich tables and JSON based on context.

### 6.5 Scripting Patterns

```bash
# Health check script
if [ "$(aster metrics app $ADDR --path ingest.queue_depth)" -gt 10000 ]; then
    alert "Queue depth critical"
fi

# Prometheus push gateway
aster metrics all $ADDR --format=prometheus | curl --data-binary @- $PUSHGATEWAY/metrics/job/aster

# Peer monitoring
aster net peers $ADDR | jq '.[] | select(.type == "relay") | .rtt_ms'

# Iterate over all peers
for peer in $(aster net peers $ADDR | jq -r '.[].node_id'); do
    echo "$peer: $(aster net peer $ADDR $peer | jq '.bytes_received')"
done
```

### 6.6 Steps

1. Add `register_net_subparser()` in new `cli/aster_cli/net.py`
2. Add `register_metrics_subparser()` in new `cli/aster_cli/metrics.py`
3. Implement connect-snapshot-print for each subcommand
4. Add `--format` flag with JSON/Prometheus/table/value formatters
5. Wire into `contract.py` main entry point
6. Add exit codes

### 6.7 Exit Criteria

- `aster net peers <addr>` returns JSON array
- `aster metrics transport <addr> --format=prometheus` returns valid Prometheus text
- `aster metrics app <addr> --path ingest.queue_depth` returns a single number
- All commands return appropriate exit codes
- Works in pipelines (`| jq`, `| curl`, etc.)

---

## 7. Phase 4: Producer Custom Metrics API

**Goal:** Let service authors publish application-level metrics that appear alongside transport and RPC metrics in the shell and CLI.

### 7.1 Design Principles

1. **Familiar primitives** — gauges, counters, histograms. Same vocabulary as Prometheus/OTel.
2. **Grouped** — Metrics are organized into named groups (e.g., `"ingest"`, `"cache"`). Groups map to VFS directories under `/metrics/app/`.
3. **Declarative** — Define metrics at service construction time. Update values from handler code.
4. **Exportable** — The MetricsStore produces snapshots suitable for JSON serialization and Prometheus text formatting.
5. **Optional** — Producers who don't publish metrics see no `/metrics/app` node. Zero overhead.

### 7.2 API Surface

```python
from aster.metrics import MetricsStore, Gauge, Counter, Histogram

# Create a store (typically one per server)
metrics = MetricsStore()

# Define metric groups
ingest = metrics.group("ingest", description="Ingest pipeline health")
ingest.gauge("queue_depth", description="Current queue depth")
ingest.gauge("batch_lag_ms", description="Time since last batch flush")
ingest.counter("events_processed", description="Total events ingested")
ingest.counter("failed_batches", description="Total failed batch writes")
ingest.histogram("batch_duration_ms", description="Batch processing time",
                 buckets=[1, 5, 10, 25, 50, 100, 250, 500, 1000])

cache = metrics.group("cache", description="LRU cache stats")
cache.gauge("size_bytes", description="Current cache size")
cache.counter("hits", description="Cache hits")
cache.counter("misses", description="Cache misses")
cache.counter("evictions", description="Cache evictions")
```

### 7.3 Updating Metrics from Handler Code

```python
@service(name="IngestPipeline", version=1)
class IngestPipeline:
    def __init__(self):
        self._queue: asyncio.Queue = asyncio.Queue()

    @rpc
    async def ingest(self, req: IngestRequest) -> IngestResponse:
        await self._queue.put(req.payload)

        # Update gauge directly
        ingest.set("queue_depth", self._queue.qsize())
        ingest.inc("events_processed")

        return IngestResponse(accepted=True)

    async def _flush_loop(self):
        while True:
            batch = await self._drain_batch()
            t0 = time.monotonic()
            try:
                await self._write_batch(batch)
                ingest.observe("batch_duration_ms", (time.monotonic() - t0) * 1000)
                ingest.set("batch_lag_ms", 0)
            except Exception:
                ingest.inc("failed_batches")
```

### 7.4 Wiring to the Server

```python
async with AsterServer(
    services=[IngestPipeline()],
    metrics=metrics,       # ← attach the MetricsStore
) as srv:
    await srv.serve()
```

When `metrics` is provided, the server:

1. Stores a reference to the `MetricsStore`
2. Exposes it via `server.app_metrics_snapshot() -> dict`
3. Makes it available to the shell/CLI via the `ObservabilityProvider`

### 7.5 MetricsStore Internals

```python
class MetricGroup:
    name: str
    description: str
    _gauges: dict[str, float]
    _counters: dict[str, int]
    _histograms: dict[str, HistogramAccumulator]

    def set(self, name: str, value: float) -> None: ...
    def inc(self, name: str, delta: int = 1) -> None: ...
    def observe(self, name: str, value: float) -> None: ...
    def snapshot(self) -> dict: ...

class MetricsStore:
    _groups: dict[str, MetricGroup]

    def group(self, name: str, description: str = "") -> MetricGroup: ...
    def snapshot(self) -> dict[str, dict]: ...
    def prometheus(self) -> str: ...
```

Thread/task safety: `_gauges` and `_counters` use atomic-style updates (simple dict writes are atomic in CPython; for correctness under true concurrency, use `threading.Lock` or `asyncio.Lock` as appropriate).

### 7.6 Snapshot Format

```json
{
  "ingest": {
    "_description": "Ingest pipeline health",
    "queue_depth": {"type": "gauge", "value": 4291},
    "batch_lag_ms": {"type": "gauge", "value": 892},
    "events_processed": {"type": "counter", "value": 12847291},
    "failed_batches": {"type": "counter", "value": 12},
    "batch_duration_ms": {"type": "histogram", "count": 94012, "sum": 1847291.3,
                          "buckets": {"1": 12000, "5": 45000, "10": 72000, ...}}
  },
  "cache": {
    "_description": "LRU cache stats",
    "hits": {"type": "counter", "value": 891204},
    "misses": {"type": "counter", "value": 12891},
    ...
  }
}
```

The `type` field enables the CLI/shell to distinguish gauges (show current value) from counters (show value + rate in watch mode) from histograms (show percentiles).

### 7.7 Prometheus Export

Custom metrics get namespaced under `aster_app_`:

```
# HELP aster_app_ingest_queue_depth Current queue depth
# TYPE aster_app_ingest_queue_depth gauge
aster_app_ingest_queue_depth 4291

# HELP aster_app_ingest_events_processed Total events ingested
# TYPE aster_app_ingest_events_processed counter
aster_app_ingest_events_processed 12847291

# HELP aster_app_cache_hits Cache hits
# TYPE aster_app_cache_hits counter
aster_app_cache_hits 891204
```

### 7.8 Steps

1. Create `bindings/python/aster/metrics.py` with `MetricsStore`, `MetricGroup`, metric types
2. Add `metrics` parameter to `AsterServer.__init__`
3. Add `server.app_metrics_snapshot()` method
4. Add `MetricsStore.prometheus()` export
5. Wire into `ObservabilityProvider`
6. Tests: metric creation, updates, snapshots, Prometheus format

### 7.9 Exit Criteria

- Producer can create a `MetricsStore`, define groups with gauges/counters/histograms
- `set()`, `inc()`, `observe()` work from handler code
- `snapshot()` returns typed JSON structure
- `prometheus()` returns valid Prometheus text
- Metrics appear in shell under `/metrics/app/<group>`
- Metrics appear in CLI via `aster metrics app`

---

## 8. Phase 5: Custom Metrics in the Shell

**Goal:** Wire Phase 4's `MetricsStore` into the VFS and display layer.

### 8.1 VFS Integration

When the shell connects, it queries the server for available metric groups. If the producer attached a `MetricsStore`, `/metrics/app/` is populated:

```
/metrics/app/
├── ingest/     "Ingest pipeline health"
└── cache/      "LRU cache stats"
```

Each group is a `NodeKind.METRICS_APP_GROUP` node. `cat` renders it as a metric table. `watch` polls it.

### 8.2 Remote Access

The shell connects as a client. It needs a way to fetch metrics from the remote server. Two options:

**Option A: Built-in RPC method.** The Aster server automatically exposes a `_aster.Metrics` internal service with methods `transportSnapshot()`, `rpcSnapshot()`, `appSnapshot()`. The shell calls these like any other RPC. This is the cleanest option — metrics travel over the same authenticated, encrypted channel as everything else.

**Option B: Out-of-band.** Metrics are fetched via a separate protocol. Adds complexity for no benefit.

**Decision: Option A.** The `_aster.Metrics` service is a system service (prefixed with `_aster.`) that the server registers automatically when `metrics` is provided. It does not appear in `/services/` (system services are hidden from the contract surface) but is callable by the shell/CLI.

### 8.3 System Service Contract

```python
@service(name="_aster.Metrics", version=1, scoped="shared")
class _AsterMetricsService:
    """Internal system service for remote metric access."""

    @rpc
    async def transport_snapshot(self, req: Empty) -> TransportMetricsSnapshot: ...

    @rpc
    async def rpc_snapshot(self, req: Empty) -> RpcMetricsSnapshot: ...

    @rpc
    async def app_snapshot(self, req: Empty) -> AppMetricsSnapshot: ...

    @rpc
    async def app_group_snapshot(self, req: GroupRequest) -> GroupSnapshot: ...
```

### 8.4 Steps

1. Implement `_aster.Metrics` system service
2. Auto-register it in `AsterServer` when metrics are available
3. Add VFS populate function for `/metrics/app`
4. Add display functions for app metric groups
5. Wire `cat` and `watch` for metric nodes
6. Hide `_aster.*` services from `/services/` listing

### 8.5 Exit Criteria

- Shell can `cd /metrics/app/ingest && cat .` on a remote node
- `watch /metrics/app/ingest` refreshes live
- `_aster.Metrics` does not appear in `ls /services`
- Works over authenticated connections (RCAN)

---

## 9. gRPC Familiarity Bridge

**Goal:** An SRE experienced with gRPC tooling should feel oriented within 60 seconds.

### 9.1 Concept Mapping

| gRPC Concept | gRPC Tool | Aster Equivalent | Aster Tool |
|---|---|---|---|
| List services | `grpcurl list` | `ls /services` | `aster service list <addr>` |
| Describe service | `grpcurl describe svc` | `describe` or `cat` | `aster service describe <addr> <svc>` |
| Invoke method | `grpcurl -d '{}' host svc/method` | `invoke` or `./method` | `aster service invoke <addr> <svc> <method> [data]` |
| Health check | `grpc_health_v1.Health/Check` | `cat /metrics/rpc` | `aster metrics rpc <addr>` |
| Channel stats | `channelz` (gRPC admin) | `ls /net/connections` | `aster net connections <addr>` |
| Peer info | `channelz` subchannels | `ls /net/peers` | `aster net peers <addr>` |
| Prometheus metrics | `/metrics` HTTP endpoint | `cat /metrics/transport` | `aster metrics all <addr> --format=prometheus` |
| Reflection | gRPC server reflection | `describe` (reads manifest) | `aster service describe <addr> <svc>` |
| Load balancing stats | Envoy admin `/clusters` | `ls /net/peers` + per-peer bytes | `aster net peers <addr>` |
| Connection count | Envoy `/stats` | `cat /metrics/transport` | `aster metrics transport <addr>` |
| App metrics (RED) | Envoy `/stats` or custom OTel | `cat /metrics/rpc` | `aster metrics rpc <addr>` |
| Custom app metrics | Custom OTel / StatsD | `cat /metrics/app/<group>` | `aster metrics app <addr>` |

### 9.2 CLI Aliases & Discoverability

The non-interactive CLI should feel natural to a `grpcurl` user:

```bash
# grpcurl-style discovery
aster service list <addr>          # like: grpcurl <host> list
aster service describe <addr> Svc  # like: grpcurl <host> describe Svc
aster service invoke <addr> Svc.method '{"key":"val"}'
                                   # like: grpcurl -d '{"key":"val"}' <host> Svc/method
```

### 9.3 Shell `help` Context

When an SRE types `help` at the root, include a one-liner:

```
Tip: Coming from gRPC? See `help grpc` for a concept mapping.
```

`help grpc` prints a condensed version of the table in §9.1.

### 9.4 Health Checking

gRPC has a standardized health check protocol (`grpc.health.v1.Health`). Aster should offer an equivalent:

The `_aster.Health` system service is automatically registered:

```python
@service(name="_aster.Health", version=1, scoped="shared")
class _AsterHealthService:
    @rpc
    async def check(self, req: HealthCheckRequest) -> HealthCheckResponse:
        """Returns SERVING, NOT_SERVING, or SERVICE_UNKNOWN."""
        ...

    @server_stream
    async def watch(self, req: HealthCheckRequest) -> AsyncIterator[HealthCheckResponse]:
        """Streams health status changes."""
        ...
```

This gives gRPC-familiar tooling a known entry point:

```bash
$ aster service invoke <addr> _aster.Health check service="IngestPipeline"
{"status": "SERVING"}
```

### 9.5 Steps

1. Add `help grpc` shell command
2. Ensure CLI noun-verb structure mirrors gRPC tooling conventions
3. Implement `_aster.Health` system service
4. Add health status to welcome banner
5. Document mapping in user-facing docs

### 9.6 Exit Criteria

- `help grpc` prints concept mapping table
- `aster service list/describe/invoke` work non-interactively
- `_aster.Health/check` returns SERVING status
- gRPC-experienced SRE can perform equivalent operations without reading docs

---

## 10. Prometheus & OTel Integration

### 10.1 Unified Prometheus Export

`aster metrics all <addr> --format=prometheus` combines all metric sources into a single Prometheus-compatible text output:

```
# === Transport (iroh) ===
# HELP iroh_send_ipv4 ...
iroh_send_ipv4 1932735488
...

# === RPC (aster.rpc) ===
# HELP aster_rpc_started Total RPC calls started
# TYPE aster_rpc_started counter
aster_rpc_started{service="IngestPipeline",method="ingest"} 98412
...

# === Application (aster_app) ===
# HELP aster_app_ingest_queue_depth Current queue depth
# TYPE aster_app_ingest_queue_depth gauge
aster_app_ingest_queue_depth 4291
...
```

### 10.2 OTel Integration

The existing `MetricsInterceptor` already uses OTel when available. The `MetricsStore` should do the same:

- When OTel is installed, each `gauge()` / `counter()` / `histogram()` call creates a corresponding OTel instrument
- `set()` / `inc()` / `observe()` update both the in-memory store (for snapshots) and the OTel instrument (for the OTel export pipeline)
- When OTel is not installed, only the in-memory store is used

This means a production deployment with an OTel collector gets full integration for free. The CLI/shell always works regardless of OTel presence.

### 10.3 Scraping Pattern

For teams that prefer pull-based Prometheus scraping rather than push:

```bash
# Lightweight cron / systemd timer
*/15 * * * *  aster metrics all $NODE --format=prometheus > /var/lib/node_exporter/aster.prom
```

Or a dedicated `aster metrics serve` command (future) that runs a tiny HTTP server exposing `/metrics` — but that's out of scope for this spec.

---

## 11. Exit Criteria

The spec is complete when:

1. **Shell:** `ls /` shows `/net` and `/metrics` alongside existing nodes
2. **Shell:** An SRE can navigate from `ls /net/peers` → peer detail → connection detail using only `cd`, `ls`, `cat`
3. **Shell:** `watch` works on `/net/peers`, `/metrics/transport`, `/metrics/app/<group>`
4. **CLI:** `aster net peers <addr>` returns JSON
5. **CLI:** `aster metrics all <addr> --format=prometheus` returns valid Prometheus text
6. **CLI:** `aster metrics app <addr> --path ingest.queue_depth` returns a scalar
7. **API:** Producers can create a `MetricsStore`, attach it to `AsterServer`, and see custom metrics in the shell
8. **gRPC bridge:** `help grpc` prints concept mapping; `_aster.Health/check` works
9. **Tests:** Integration tests cover metric creation, remote snapshot, CLI output formats

---

## 12. Dependency Map

```
Phase 1 (VFS nodes)
  │
  ├──▶ Phase 2 (watch) — needs VFS nodes to exist
  │
  ├──▶ Phase 3 (CLI automation) — needs display/snapshot functions from Phase 1
  │
  └──▶ Phase 5 (custom metrics in shell) — needs VFS infrastructure
          │
          └── Phase 4 (MetricsStore API) — must exist before Phase 5

Phase 4 (MetricsStore API) — independent, can start in parallel with Phase 1

Phase 9 (gRPC bridge) — independent, can start anytime
    └── _aster.Health service depends on system service infrastructure (Phase 5, §8.3)
```

**Recommended order:** Phase 4 and Phase 1 in parallel → Phase 2 → Phase 3 → Phase 5 → Phase 9.

---

## Appendix A: System Services Convention

Services prefixed with `_aster.` are **system services** — automatically registered by the framework, hidden from `/services/` listings, not part of the producer's contract surface, but callable by authorized clients.

| Service | Purpose | Auto-registered when |
|---------|---------|---------------------|
| `_aster.Metrics` | Remote metric snapshots | `metrics=` provided to AsterServer |
| `_aster.Health` | Health checking (gRPC-compatible semantics) | Always |

System services:
- Do not appear in `ls /services` or contract manifests
- Are not subject to producer-defined interceptors (they use framework interceptors only)
- Require the same authentication (RCAN) as regular services
- Use the `_aster/` wire type namespace for request/response types

This convention allows the framework to grow its management surface without polluting the producer's API namespace.

---

## Appendix B: `grpcurl` → `aster` Cheat Sheet

For inclusion in `help grpc` output:

```
grpcurl <host>:443 list                  →  aster service list <addr>
grpcurl <host>:443 describe MyService    →  aster service describe <addr> MyService
grpcurl -d '{"name":"hi"}' <host>:443 \
  mypackage.MyService/SayHello           →  aster service invoke <addr> MyService.sayHello '{"name":"hi"}'
grpc_health_v1.Health/Check              →  aster service invoke <addr> _aster.Health check
grpc_cli ls <host>                       →  aster service list <addr>
grpc_cli call <host> Method '{"k":"v"}'  →  aster service invoke <addr> Svc.Method '{"k":"v"}'
```
