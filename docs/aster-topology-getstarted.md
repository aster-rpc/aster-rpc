# Topology: locality-aware swarms with `aster.net.Topology`

Every Aster node can build a live picture of **where its peers are** — same
LAN, same region, far away — and swarms can self-organise into **clusters of
nearby nodes** with an elected **bridge** per cluster for cross-cluster work.
No coordinator, no config file describing your network: nodes measure,
share measurements, and every node independently derives the *same* answer.

> **Status: v1 + v2 core shipped (Rust, 2026-07-04).** The local per-peer
> view (§3) and the shared swarm — clusters, bridges, `separated()` (§4) —
> are implemented in the Rust `aster` crate; other bindings follow the FFI
> wave. Still to come as v2.x
> ([design doc](_internal/working_ideas/aster-network-topology.md)):
> prefix-hash L1 candidates, witness join-probing and maintenance probes
> (today's swarm measures organically-connected peers only), and the ASN
> hint.

---

## 1. What you get

Three things, from cheapest to most powerful:

1. **A proximity view of every peer** — a *locality ladder* level per peer:

   ```
   L0 same-host   L1 same-lan   L2 same-site   L3 same-region   L4 far
   ```

   plus live **path quality** (RTT, loss, throughput estimate) and a
   **confidence** score saying how much real measurement backs the entry.

2. **Clusters** — the swarm partitions itself into groups of mutually-close
   nodes. Every node computes the same partition from the same shared
   measurements, so "my cluster" is an agreed fact, not a local guess.

3. **A bridge per cluster** — one member, deterministically elected, that
   handles cross-cluster work (cross-region replication, fan-in
   aggregation). Every member knows whether it is the bridge without any
   election protocol running.

The canonical use case: **six nodes doing file replication, three per
region**. They self-organise into two clusters of three; members replicate
densely inside their cluster; the two bridges replicate across regions.
Nobody configured that layout — it was derived, and it re-derives itself
when a node dies or a new one joins.

### Producer nodes only

Topology is for **producer nodes** — the nodes *you* operate, admitted
under one root identity, that work together (replication peers, edge
services). **Consumer nodes** — nodes that merely use your swarm's
services — never participate: they aren't admitted to the topology
namespace, publish nothing, and never appear in a cluster. For example, in
portal-desktop the edge service is a producer while every host is a
consumer; in portal-sync every node under a single root identity is a
producer (and future externally-invited sync nodes will be consumers).
This is a trust boundary: topology assumes its members are your own
semi-trusted cooperators. Steering a *consumer* to its nearest producer is
a separate future feature, not something you get by admitting consumers.

## 2. The mental model

Three separate outputs, because they answer different questions:

| Output | Question | Moves | Use it for |
|---|---|---|---|
| Ladder / cluster | *Where* is this peer? | Slowly (sticky, minutes) | Structure: who's a replica candidate, who anchors a site |
| Path quality | How good is the link *right now*? | Fast | Selection: which nearby peer to fetch from now |
| Confidence | How sure are we? | With measurement | Policy: prefer the certain path over the maybe-faster one |

Two rules worth internalising:

- **Structure ignores link health.** A same-LAN peer behind a saturated
  switch is still same-LAN. Pick *candidates* by position, pick *among
  candidates* by path quality.
- **Closeness is measured, not claimed.** A peer cannot talk its way into
  your cluster: cluster edges require *both* sides to have measured the
  pair close, and LAN status requires a verified private-path dial. Claims
  (subnets, ASNs) only generate candidates to check.

## 3. Reading your node's view (v1 — shipped in Rust)

One prerequisite: start the node with monitoring enabled. From there the
view is fed passively by connections your node already makes (RPC,
replication, gossip) — no probe traffic:

```rust
use aster::topology::LadderLevel;

let config = AsterConfig::builder().monitoring(true).build();
let node = Node::start(config).await?;

let topo = node.topology();               // local view, no RPC
for peer in topo.peers() {
    println!(
        "{}  {:?} ({})  rtt≈{:?}µs  confidence={}ppm",
        peer.node_id, peer.level, peer.level_reason, peer.rtt_us, peer.confidence_ppm,
    );
}
```

Each `PeerView` carries the ladder level *plus the signal that justified
it* ("loopback path", "verified private path", …), smoothed RTT and
jitter, loss rate, a passive throughput estimate with a cwnd/RTT ceiling,
direct-vs-relay, and a confidence score that grows with samples and
decays with staleness. Levels are sticky: a change must hold for ~30
seconds before the level moves, so DHCP churn and RTT jitter don't flap
your structure.

Typical v1 uses:

```rust
// Nearest provider first for a blob fetch: closest level, then lowest RTT.
candidates.sort_by_key(|id| {
    topo.peer(id)
        .map(|v| (v.level, v.rtt_us.unwrap_or(u64::MAX)))
        .unwrap_or((LadderLevel::Far, u64::MAX))
});

// Only consider LAN-or-closer peers for a chatty sidecar:
let lan: Vec<_> = topo.peers().into_iter()
    .filter(|p| p.level <= LadderLevel::SameLan)
    .collect();
```

There's also an **opt-in RPC surface** — the baseline `aster.net.Topology`
service (`peers()` / `peer(node_id)`) — for inspecting a producer node
remotely:

```rust
let srv = AsterServer::builder()
    .config(config)                                     // monitoring(true) required
    .builtin_topology(true)                             // off by default
    .topology_requires(require_role("operator"))       // and usually gated
    .start()
    .await?;
```

Unlike `aster.ops.NodeInfo` it is **disabled by default** — the view
discloses inferred network layout — and its wire payload omits peer socket
addresses (those stay in-process only).

In v1 the view covers **peers you've actually connected to**. Ranking
never-contacted peers arrives with v2's shared measurements.

## 4. Clusters and bridges (v2 — shipped in Rust)

v2 adds a shared **topology document** (an iroh-docs namespace) where every
producer publishes its measurements, and from which every producer
independently derives the *same* clusters and bridges. The namespace secret
is distributed like any Aster capability — inside a
[sealed grant](aster-sealed-grants-getstarted.md):

```rust
use aster::grants::{open_namespace_grant, seal_namespace_grant, GrantContext};
use aster::topology::TopologySwarmOptions;
use aster::{NamespaceCapability, NamespaceSecret, SecretKey};

// Root: mint the swarm's topology namespace secret once, then seal it
// per admitted node.
let topo_secret = NamespaceSecret::from_bytes(SecretKey::generate().to_bytes());
let ctx = GrantContext {
    app: "myapp/topo/v1",
    granter: &root_id,
    recipient: &member_id,
    resource: &topo_secret.id().to_bytes(),
    path: "/topo/v1/grants",
    role: "write",
};
let sealed = seal_namespace_grant(&ctx, &NamespaceCapability::write(topo_secret.clone()))?;

// Member: open the grant with its identity key and join the swarm.
let NamespaceCapability::Write(secret) =
    open_namespace_grant(&identity_secret, &ctx, &sealed)?
else { unreachable!() };
node.topology()
    .enable_shared(&secret, TopologySwarmOptions::default())
    .await?;
```

From then on the node heartbeats its `position` record and one RTT-edge
half per measured peer into the doc, and derives the swarm view locally:

```rust
let topo = node.topology();
if let Some(cluster) = topo.my_cluster().await? {
    // The replication pattern from the use case above:
    for peer in &cluster.members {
        replicate_with(peer); // dense, intra-cluster
    }
    if cluster.bridge == node.id().to_string() {
        for other in topo.clusters().await? {
            if other != cluster {
                replicate_with(&other.bridge); // bridge ↔ bridge, cross-region
            }
        }
    }
}
```

The bridge check is just a string comparison — election is a deterministic
function of the shared records (a blake3 ranking salted by the namespace
id), so every member evaluates it identically. If the bridge dies, its
records stop refreshing and, within the liveness TTL, every node
independently promotes the same successor.

You can also ask for separation evidence:

```rust
use aster::topology::SeparationVerdict;

match topo.separated(&node_a, &node_b).await? {
    SeparationVerdict::Separated => { /* measured far apart — real diversity */ }
    SeparationVerdict::Connected => { /* same cluster */ }
    SeparationVerdict::Unknown   => { /* not enough measurements — treat as unknown, not far */ }
}
```

`Unknown` is a real answer: absence of a measurement is never treated as
evidence of distance — `Separated` requires corroborated far measurements
under a coverage rule.

The opt-in `aster.net.Topology` RPC service gains `clusters()` and
`separated(node_a, node_b)` alongside the v1 `peers()`/`peer()` methods,
so operators can inspect a producer's derived view remotely.

Two practical notes:

- **Timing knobs**: production defaults (60s heartbeat, 2min edge hold,
  5min liveness) come from the design doc; `TopologySwarmOptions.timing`
  overrides them (tests run with sub-second refresh).
- **Custom admission**: `TopologySwarmOptions.admitted` installs a
  read-time filter over record authors. Without it, every holder of the
  namespace secret counts — which is already admission-gated by how you
  distributed the grants.

## 5. What to rely on — and what not to

**Rely on:**

- Cluster membership for *structure* — replica candidate sets, "one
  aggregator per site", locality-aware fetch order.
- `Separated` verdicts for failure-domain diversity — they're backed by
  corroborated far measurements, not inference.
- The bridge for *idempotent* cross-cluster work (replication sync,
  metrics fan-in).

**Do not rely on:**

- **The bridge being exclusive.** During churn two nodes can briefly both
  act as bridge, or neither. Replication tolerates this (a duplicate sync
  is wasted bandwidth, not corruption). If you need a single writer or a
  lock, put a real coordination primitive on top — topology does not
  provide one.
- **Fast failover.** Bridge succession is bound by record TTLs —
  minutes-scale by default. Latency-critical failover needs its own
  liveness layer.
- **Clusters as geography.** A cluster means "measured mutually close",
  nothing more. Two clusters that are *actually* one site can appear
  briefly after a cold start (a "false split") — they merge automatically
  as measurements accumulate, and replication keeps working through the
  bridges meanwhile.
- **Topology as a consumer feature.** Only producer nodes participate.
  Don't admit a node to the topology namespace just so it can *see* the
  swarm's layout — admission makes it a member of your trust boundary
  (see the privacy note below).

**Privacy note:** nodes publish salted hashes of their private subnet
prefixes (to find LAN candidates) into the admission-gated topology doc.
Swarm members can, with effort, recover common RFC1918 prefixes from
these — the protection is against outsiders and cross-swarm correlation,
not against fellow members. Don't admit nodes you wouldn't tell your
subnet layout.

## 6. Tuning

Defaults are chosen for "LAN vs metro vs region" distinctions and should
just work. The knobs you're most likely to touch:

| Knob | Default | When to change |
|---|---|---|
| `cluster_threshold` | 15 ms | Your "one cluster" means a metro area (raise) or a single DC (lower) |
| `liveness_ttl` | 5 min | Faster bridge failover vs more heartbeat writes |
| `witness_r` | 3 | Larger clusters / more separation evidence |

The full normative table (hysteresis bands, hold times, TTLs, coverage
rule) lives in the
[design doc](_internal/working_ideas/aster-network-topology.md#defaults-normative-config-tunable).

## 7. Roadmap

- **v1 — local view. Shipped (Rust).** Ladder + path quality + confidence
  over live connections; `Node::topology()` + opt-in `aster.net.Topology`.
  No shared state. Python/TS/Java surfaces follow the FFI wave.
- **v2 — clusters. Core shipped (Rust).** Shared topology doc, corroborated
  edge rules, cluster derivation, bridges/witnesses, `separated()`,
  sealed-grant secret distribution, read-time admission filter. Remaining
  v2.x: prefix-hash L1 candidates, witness join-probing + maintenance
  probes, ASN hint.
- **v3 — consumers.** Locality-aware gossip biasing, blob-fetch
  strategies, leader-per-LAN aggregation, replica-placement policies built
  on the view.

Design rationale, trust model, and the normative wire schema:
[`docs/_internal/working_ideas/aster-network-topology.md`](_internal/working_ideas/aster-network-topology.md).
