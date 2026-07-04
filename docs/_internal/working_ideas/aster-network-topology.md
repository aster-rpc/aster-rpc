# Network Topology Self-Mapping — Locality-Aware Aster Swarms

> **Status: implementation-ready draft (2026-07-04) for v1 and a feature-flagged v2.** The signal trust rules, edge rules, cluster derivation, witness/coverage policy, bridge election, record schema, and defaults table are **normative** for v2. Numeric defaults are chosen-and-tunable (config, not folklore left open). Open questions now cover only v3 and scale-out.

**Companion docs:**
- [../aster-baseline-services.md](../aster-baseline-services.md) — the topology surface lives in the `aster.*` baseline catalog (`aster.net.Topology`, adjacent to `aster.ops.Connections`)
- Local discovery plan (mDNS + `AddrFilter`) — mDNS sightings are a Tier-1 feeder for this design, not a standalone feature

---

## Why

**The goal: cluster together nodes that are close together.** Nodes that want to self-organise — pick replication partners, build aggregation trees, prefer local peers for blob fetch — need a shared, agreed notion of "who is near whom", computed without a coordinator: every node derives the same clusters from signals it can observe, verify, or corroborate.

**Motivating use case.** Six nodes doing file replication: three in one region, three in another. They should self-organise into two clusters of three that replicate amongst themselves, with one node per cluster elected as the *bridge* that replicates cross-region. No node is told this layout — every node derives it, and all six derive the *same* answer.

**Scope: producer nodes only (ratified 2026-07-04).** Topology — the shared doc, clusters, bridges — is for **producer** nodes: cooperating nodes admitted under a single root identity that work together. **Consumer** nodes — nodes that merely *use* a swarm's services — are never admitted to the topology namespace: they don't publish records, aren't vertices in the cluster graph, and never appear in a `ClusterView`. Concretely: in portal-desktop the edge service is a producer; every host is a consumer. In portal-sync, every node under a single root identity is a producer (no consumer nodes today; future external nodes invited to sync a particular tree will be consumers). This is a trust boundary, not an implementation stage — everything here (shared namespace secret, member-visible prefix hashes, corroborated edges, TTL-bound liveness) assumes members are semi-trusted cooperators, which consumers are not. Consumer-side locality (steering a consumer to its nearest producer) is a different problem with a different mechanism (a server-side hint from the producers' view), out of scope for this design. Throughout this doc, "admitted" means producer admission.

Three questions, in increasing distance:

1. Which nodes are on the same private network / LAN?
2. Which nodes are "nearby" — same site, metro, or region?
3. Which nodes are distant (another region/geography)?

## Scale stance: measure directly, don't estimate

Aster swarms are small-to-medium — replication groups of single digits, swarms of hundreds, maybe low thousands. At that scale the pairwise RTT picture is cheap: most of it falls out of connections nodes make anyway, and the join burst plus low-rate maintenance covers the rest. So this design **measures distances directly and shares the measurements**; it does not estimate distances between never-contacted nodes via network coordinates (see Ruled out). The shared map is the raw RTT edge set, not an embedding of it.

## Signal inventory

Most raw signals already exist in the stack (iroh path info, quinn stats, planned mDNS). The design work is the inference layer, not new measurement machinery.

| Signal | Proves | Cost | Trust class |
|---|---|---|---|
| Direct QUIC conn on a private addr | Same routable private network (handshake proves peer identity — unspoofable) | Free — iroh path info | **Verified** |
| mDNS sighting | Same L2/broadcast domain | Free — planned local-discovery feature | **Verified** |
| Live QUIC RTT (smoothed) | Latency distance — **the primary clustering signal** | Free on active connections | **Measured** |
| Published RTT edge, corroborated by both endpoints | Latency distance between two *other* nodes | Replicated record ×2 | **Corroborated** (see edge rules) |
| Observed goodput / congestion window during real transfers | Path throughput ceiling | Free — quinn `Connection::stats()`, passive | **Measured** |
| QUIC loss / jitter / congestion-event counts | Path quality (not position) | Free — quinn `Connection::stats()` (noq exposes no PTO counter; congestion events + black-hole detections are the stand-in) | **Measured** |
| Path migration history, relay-vs-direct | Path stability | Free — iroh path info | **Measured** |
| Same observed public egress IP | Same NAT/site | Free — relay-observed address | Semi-verified |
| Same home relay | Weak region hint (relays are holepunch fallbacks — affinity is incidental, never a cluster key) | Free | Advisory |
| IP → ASN lookup | Provider / network operator | ~10MB embedded offline DB (iptoasn-style) | Advisory (DB is real, IP is claimed) |
| Claimed interface/prefix list | Candidate same-LAN peers | Replicated record | **Claim only** |
| Gossip `PeerData` locality summary | Peer's claimed egress/ASN, delivered with membership | Free — piggybacks on HyParView messages (see gossip section) | **Claim only** |

**Core rule: claims generate candidates; verified/measured/corroborated signals make decisions.** Anything that drives data placement or authority (bridge election, replica set membership) must rest on the top tiers. A malicious node can claim any subnet or ASN; it cannot fake a completed QUIC handshake on a private path, it cannot make *your* measured RTT smaller, and it cannot manufacture the other endpoint's half of a corroborated edge.

### Ruled out

- **Vivaldi / network coordinate systems.** Coordinates exist to avoid O(N²) probing across thousands of nodes — a problem this design doesn't have at Aster swarm scale. The research field is essentially dormant; the one production survivor (Serf/Consul network tomography) serves fleets far larger and looser than ours. Coordinates are also the weakest trust class (claimed, gameable embeddings), whereas shared RTT edges are corroborated measurements. If a future deployment mode genuinely reaches coordinate scale, revisit — with Serf's parameterisation, not the original paper's.
- **Relay affinity as cluster key.** Relays are fallbacks for failed holepunching; most connections are direct, and relay choice is a path artifact, not a location. Home relay survives only as an advisory hint.
- **True BGP path data.** AS-paths live in router RIBs; nodes can't observe them without external feeds, and AS-path length correlates poorly with latency anyway.
- **Traceroute-style probing.** Raw ICMP needs privileges, breaks on Windows/containers, adds little over RTT.
- **Raw private-IP comparison as a conclusion.** RFC1918 space is reused everywhere; `10.0.1.5/24` on two nodes proves nothing. Same-prefix match is a *candidate generator* whose confirmation is a verified private dial.

## The locality ladder

Each level derives from named signals; a node assigns every known peer the highest level it can justify:

```
L0 same-host    loopback / identical observed addrs
L1 same-lan     verified private dial  ∨  mDNS sighting
L2 same-site    same public egress IP, no private path (e.g. separate VLANs, one office)
L3 same-region  RTT under cluster_threshold (own measurement, or corroborated edge)
L4 far          everything else
```

RTT bands (defaults, see Defaults table): <1ms same host/rack, <2ms LAN/DC, <15ms metro/region cluster threshold.

Levels must be **sticky** — hysteresis on minutes scale, smoothed RTT (QUIC's own smoothed-RTT estimator is the right input), and cluster-membership decisions consume the smoothed view only. Otherwise DHCP churn and RTT jitter make the hierarchy flap.

## Clusters — the derived objects

The ladder is a per-peer view. Clusters are the *shared* structure derived from it — the objects the replication use case actually consumes.

### Derivation

Build a graph whose **vertices are admitted nodes that are live** — position record fresher than `liveness_ttl` — so a dead node leaves every cluster within `liveness_ttl` (300 s), not the longer `edge_ttl` (900 s) its edges would otherwise grant it. An edge exists between two vertices when their RTT is under the cluster threshold **and both endpoints' published measurements agree** (see edge rules below). **Clusters are the connected components of that graph.**

Because every node derives components from the same replicated edge set using the same pure function, all nodes converge on the same partition — agreement without consensus, without a coordinator, and without publishing any derived view.

Be precise about what a cluster *is*: a **known-close component** — nodes with corroborated evidence of mutual proximity — not a proven geographic or site partition. A missing measurement can split one real site into two clusters (**false split**); that is the normal, benign failure mode (two clusters that should be one still replicate correctly, just with an unnecessary bridge hop), shrunk by witness maintenance rather than eliminated. Consumers that need actual *separation* evidence use the coverage rule below — absence of an edge proves nothing.

**Linkage rule (pinned for v2): connected components.** Complete-linkage (every pair within threshold) is the documented escape hatch if real topologies chain two regions through a middle node (A–B 12ms, B–C 12ms, A–C 70ms); rare at region granularity, revisit only on evidence.

### Bridge election and witness sets — exact rules

Every node carries a swarm-stable score:

```
score(n) = BLAKE3( namespace_id ‖ "aster.topo.bridge.v1" ‖ node_id )
```

- `namespace_id` — the 32-byte iroh-docs NamespaceId of the topology doc. This **is** the swarm id: unique per swarm, already known to every admitted member, nothing new to distribute.
- `node_id` — the node's 32-byte Ed25519 public key.
- Scores compare as 32-byte big-endian unsigned integers; higher wins. Tie-break (theoretical): lower `node_id` in byte order wins.

The score is deliberately independent of cluster composition, so membership changes never reshuffle rankings. Within a cluster:

- **Bridge** = highest-scoring *live* member.
- **Witness set** = top `witness_r` live members by the same score (so the bridge is always a witness). `witness_r = min(3, |members|)`.

**Liveness is a shared input** — gossip neighbor state is local and partial (active view caps at 5), so it cannot be the election input or nodes elect different bridges indefinitely. A member is **live iff its `position` record is fresher than `liveness_ttl`**; refreshing the record (every `position_refresh`) is the heartbeat. Gossip `NeighborDown` remains a local fast-path hint for a consumer's own failover behaviour, never for the election itself.

**Failover latency is TTL-bound**: a dead bridge is replaced within `liveness_ttl` + doc-sync propagation — minutes-scale. That is acceptable for replication bridging (the consumer this design serves); a consumer needing sub-minute failover must layer its own liveness on top, it cannot get it from this election.

Properties that fall out:

- Scores don't depend on the member set, so adding or removing a non-bridge member doesn't move the bridge.
- Bridge death promotes the next rank once its records decay — no protocol round.
- The fixed random priority avoids "lowest ID always does the work" bias without per-cluster inputs.

### Maintained edges — which edges must exist and stay fresh

- **Small clusters (`|members| ≤ full_mesh_cutoff`)**: full mesh — every member maintains a fresh corroborated edge to every other member.
- **Large clusters**: every member maintains fresh corroborated edges to **all witnesses**. The witness star keeps the component connected; non-witness pair edges are welcome (organic traffic produces them) but not required.
- **Cross-cluster**: each **bridge** maintains fresh measurements (near or far) to **every other cluster's witness set**. These are the missed-merge detector and the separation evidence — if any cross measurement comes under the enter threshold and holds, the clusters merge; that is false-split healing, not an anomaly.

Per-node maintenance load: O(`witness_r`) for members, plus O(`witness_r` · #clusters) for bridges. Bounded and small.

### Separation — the coverage rule

`separated(X, Y)` holds iff:

1. no qualifying edge exists between any pair spanning X × Y, **and**
2. at least `effective_coverage = min(separation_coverage, |X| · |Y|)` **corroborated far pairs** span X × Y — a pair counts only when *both* halves are fresh (≤ `edge_ttl`) and both report `rtt_us ≥ hyst_exit` (the bridge→witness maintenance measurements normally supply these).

Anything less is `UNKNOWN`, not `SEPARATED`. Default `separation_coverage = 2`; the `min` clamp handles small clusters — singleton-vs-singleton has exactly one pair, and one corroborated far pair is still two independent measurements (one per endpoint), so it suffices rather than being permanently `UNKNOWN`.

### Agreement boundary

Deterministic derivation gives **eventual** agreement. During churn (a node joining, an edge crossing the threshold, records decaying at slightly different reader clocks), two nodes can transiently disagree about membership or bridge identity — meaning briefly zero or two cross-region bridges. For replication this is benign: sync is idempotent; a duplicate transfer wastes bandwidth, a missing one delays convergence. **Any consumer that hangs an *exclusive* role on the bridge (single writer, lock holder) needs real coordination on top — this layer does not provide it.**

### Stability

- **Hysteresis on edges — applied by the publisher, not the reader.** If each reader applied hold-time locally from when *it* first observed a record, views would diverge. Instead the measuring node tracks qualification itself and stamps `held_since_unix_ms` — set when smoothed RTT first drops below `hyst_enter`, cleared only when it rises above `hyst_exit`; graph inclusion is then a pure rule over record fields: both halves held for at least `min_hold`. The asymmetric enter/exit band means a pair hovering at the boundary cannot flap the partition. Residual reader divergence is bounded by clock skew and falls under the eventual-agreement boundary.

## Defaults (normative config, tunable)

| Constant | Default | Meaning |
|---|---|---|
| `cluster_threshold` | 15 ms | same-cluster RTT bound (nominal) |
| `hyst_enter` | 12 ms (0.8 ×) | publisher starts holding (`held_since` set) only below this |
| `hyst_exit` | 18 ms (1.2 ×) | publisher clears holding (`held_since` → 0) only above this; reader's qualification bound |
| `min_hold` | 120 s | continuous hold before an edge enters the graph |
| `position_refresh` | 60 s | heartbeat: rewrite own `position` record |
| `liveness_ttl` | 300 s | member live iff position record fresher than this |
| `edge_ttl` | 900 s | edge half ignored if `measured_unix_ms` older than this |
| `maintenance_interval` | 60 s | required edges with no organic sample this recent get probed |
| `witness_r` | min(3, cluster size) | witness-set size per cluster |
| `full_mesh_cutoff` | 8 members | at or below: full-mesh maintained edges |
| `separation_coverage` | 2 | distinct far pairs required for `separated` |
| `max_clock_skew` | 30 s | records timestamped beyond now + skew are dropped |

**Reader time rules** (one rule set, reader's clock throughout):

- drop any record with a timestamp > now + `max_clock_skew` (future-dated);
- member live iff now − `updated_unix_ms` ≤ `liveness_ttl`;
- edge half fresh iff now − `measured_unix_ms` ≤ `edge_ttl`;
- edge qualifies iff both halves fresh ∧ both `held_since_unix_ms` ≠ 0 ∧ now − `held_since_unix_ms` ≥ `min_hold` on both ∧ both `rtt_us` ≤ `hyst_exit`.

**The band belongs to the publisher**: it sets `held_since` only when smoothed RTT first drops below `hyst_enter`, and clears it (→ 0) only when RTT rises above `hyst_exit`. The reader's `≤ hyst_exit` check merely rejects inconsistent records (nonzero `held_since` alongside an above-exit RTT); the reader must **not** test against `hyst_enter` — doing so would drop edges the moment RTT drifts into the 12–18 ms band and defeat the hysteresis.

All TTLs are ≫ `max_clock_skew`, so clock-skew divergence between readers is transient and bounded. Nodes are expected NTP-synced; a node outside skew self-excludes (its records get dropped), which is detectable via `aster.ops`.

## Position vs path quality — two outputs, not one

The ladder and clusters answer *where a node is*. They deliberately ignore *how good the link is right now* — same-LAN behind a saturated switch is still same-LAN. Consumers usually need both, so the view exposes two per-peer outputs:

1. **Ladder level / cluster membership** — topological position. Sticky, slow-moving, drives structure (who is a candidate replica, who anchors a LAN).
2. **Path quality** — a per-edge health score over the live connection: smoothed RTT, loss rate, jitter, congestion events, **throughput estimate**, relay-vs-direct, migration churn. Fast-moving, drives *selection among structural peers* (which cluster member to fetch a blob from right now).

**Throughput is the component RTT can't stand in for.** Bulk replication cares about bandwidth; a 5ms link can be a saturated 100Mbps uplink while a 12ms link is 10Gbps. Estimate it passively: observed goodput during real transfers (replication traffic is its own probe) plus quinn's congestion window as a ceiling (cwnd / RTT ≈ what the path will bear). No dedicated probe traffic.

Node-level capacity (disk headroom, load, uptime) also matters for picking replication partners, but it is not topology — it belongs in `aster.ops.NodeInfo` and gets consumed *alongside* this view.

No fixed weighted-sum formula — the right combination is consumer-specific (blob fetch cares about loss × throughput; RPC routing cares about RTT tail; gossip cares about stability). The view publishes the raw smoothed components; consumers combine.

Note the inversion for durability: low RTT is a *negative* signal for replica diversity — 1ms apart usually means same rack, same power strip, same failure domain. That's exactly why the replication use case spreads across clusters and bridges between them.

### Confidence

Every per-peer entry carries a **confidence** alongside its estimate — how much measurement backs it:

```
PeerView {
  peer_id
  ladder_level        # + the signal that justified it
  cluster             # membership per the derived partition
  est_rtt             # own smoothed measurement, or from corroborated edges
  path_quality        # None if never connected
  confidence          # ~0.99 = continuously measured; lower for edge-derived; low for claim-derived
  last_measured
}
```

Own continuous measurement saturates confidence toward 1; corroborated edges published by others rank below that; claims (prefix hashes, ASN) bottom out the scale. Policies can then express "prefer the certain path over the maybe-faster one" — e.g. replica placement keeps a measured 20ms peer over an edge-reported 12ms one until confirmed by a dial.

## Shared topology doc — the replication substrate

Topology knowledge replicates through a dedicated **iroh-docs namespace**. The root node provisions the namespace and propagates the namespace secret to admitted nodes via **sealed grants** — the per-recipient HPKE capability-distribution mechanism generalized from portal-sync, now implemented in `aster::grants` (see [../aster-sealed-grants.md](../aster-sealed-grants.md)). Nodes write their own observations; the CRDT store replicates them swarm-wide with no coordinator and no requirement that nodes be online simultaneously.

**Attribution survives the shared secret.** iroh-docs entries are signed by both the namespace key *and* the per-node author key — a shared write secret does not mean anonymous writes. Every record stays attributable to the node that wrote it, which is what keeps the trust tiers enforceable: each RTT edge is attributable to its measurer; each claim to its claimant.

### Record schema (normative for v2)

**Encoding**: Fory, registered under payload root `aster.topo` in the PayloadRegistry (collision-safe per registry rules). All integers explicit-width. **Key layout** (version in the path; unknown `topo/v<N>` prefixes are ignored for forward compatibility — a version bump is a new prefix, readers may dual-read during migration):

```
topo/v1/<node_id_hex>/position           → NetworkPosition
topo/v1/<node_id_hex>/rtt/<peer_id_hex>  → RttEdge   (this node's half of a measured edge)
topo/v1/<node_id_hex>/lan/<peer_id_hex>  → LanEdge   (verified private-path dial attestation)
```

```
NetworkPosition {
  node_id:          bytes(32)       # Ed25519 public key
  observed_public:  string          # relay-observed public endpoint; "" = unknown
  asn:              int32           # 0 = unknown (advisory)
  home_relay:       string          # "" = none (advisory)
  prefix_hashes:    list<bytes(16)> # truncated BLAKE3, see Privacy
  updated_unix_ms:  int64
}

RttEdge {
  rtt_us:              int32        # smoothed RTT, microseconds
  samples:             int32
  held_since_unix_ms:  int64        # 0 = not currently under hyst_enter
  measured_unix_ms:    int64
}

LanEdge {
  verified_unix_ms: int64
}
```

**Decode/drop rules**: an entry is dropped (and counted in metrics, never fatal) if the value fails Fory decode, if any timestamp is future-dated beyond `max_clock_skew`, or if attribution fails validation (below). Unknown Fory fields are tolerated (compatible mode) — additive evolution doesn't need a version bump; semantic changes do.

**Edge rules.** An edge enters the cluster graph only when **both endpoints have published their half and both halves pass the reader time rules (fresh, held ≥ `min_hold`, `rtt_us` ≤ `hyst_exit`); the graph uses the max of the two sides.** A one-sided record is a candidate, not an edge — a liar cannot manufacture the other endpoint's half. (Two *colluding* nodes can fake an edge between themselves; see poisoning bounds.)

Nodes publish `RttEdge` records for **every pair they measure, near or far** (`held_since = 0` for far). An above-threshold measurement is positive evidence of *separation* — which the mere absence of an edge is not — and is what the coverage rule consumes.

### Join flow

A node joining the swarm:

1. Gets admitted; receives the namespace secret; syncs the topology doc.
2. Reads all `position` records and the RTT edge set → sees the existing cluster structure *before probing anything* (cold-start solved).
3. Matches its own private prefixes against published `prefix_hashes` → L1 **candidates**; confirms each by private-path dial (cheap, and the QUIC handshake verifies identity).
4. Probes each existing cluster's **witness set** — O(`witness_r` · #clusters) dials, not O(N) — to find which cluster it belongs to (or that it starts a new one).
5. Measures per the maintained-edges policy within its own cluster, publishes its `RttEdge` halves — near *and* far: the above-threshold cross-cluster probes from step 4 are separation evidence, not waste — and its `position` record.

### Measurement maintenance

Organic traffic (replication, RPC, gossip connections) supplies most samples for free — quinn's smoothed RTT on any live connection counts. The **required** edges (member→witnesses, bridge→foreign witnesses, full mesh in small clusters) additionally get a freshness guarantee: if a required edge has no organic sample within `maintenance_interval`, the node probes it — a QUIC PING on an existing connection, else a short-lived dial. So the honest statement is **passive-first with bounded maintenance probing**, not "no probe traffic": the maintenance budget is O(`witness_r`) probes per member per interval, plus O(`witness_r` · #clusters) for bridges, and only for edges organic traffic didn't already cover.

### Trust & revocation

- **Readers validate attribution.** An entry counts only if the signing author key, the `<node_id>` path segment, and any embedded `node_id` field all match — "writes under its own author key" is a reader-enforced validation rule, not an intention. Mismatched entries are dropped.
- **Readers filter by admission.** Records from authors that are not currently admitted are ignored at read time, regardless of what's in the doc.
- **Shared secret can't be revoked per-node.** A revoked node still holds the namespace secret; admission gating stops peers syncing *with* it, but records it wrote earlier persist until overwritten. On revocation events the root rotates the namespace and redistributes — acceptable for an internal swarm; noted as a known limitation.
- **Poisoning bounds.** A single lying node can corrupt only candidates and its *own* halves of edges — corroboration blocks unilateral fake edges, so it cannot pull itself into a cluster whose members don't measure it close, and it cannot manufacture verified L1 edges to peers it can't reach. Two colluding nodes can fake an edge between themselves, which can chain-merge two clusters (mitigated by complete-linkage if needed); collusion cannot fake edges involving honest nodes. Every consequential decision re-verifies by dialing.

## Privacy

Publishing raw internal prefixes leaks site topology to the whole swarm. Publishing **salted hashes of (prefix, netmask)** is only a partial fix, and should be stated honestly: RFC1918 space is low-entropy, so any *admitted member* (who holds the salt) can dictionary the common prefixes and recover most of them. What the salt actually buys is protection against non-members and against precomputed/cross-swarm correlation — within the swarm it is obfuscation, not secrecy. Acceptable for an internal swarm because the namespace is admission-gated (members are semi-trusted; they learn candidate subnets, not the full address plan); position and edge records never leave that namespace.

**Salt derivation (pinned)**: `salt = BLAKE3(namespace_id ‖ "aster.topo.prefix-salt.v1")` — derivable by every member, nothing to distribute, unique per swarm. `prefix_hashes` entries are the first 16 bytes of `BLAKE3(salt ‖ prefix_bytes ‖ netmask_byte)`. Per-epoch rotation, if ever needed, is a v2 suffix in the derivation string.

## Consumers — what the hierarchy does with clusters

The point of the structure is to drive self-organisation:

- **Cluster-scoped replication** — the motivating use case: replicate densely within a cluster; bridges replicate across clusters. Selection *within* the cluster (who to pull from now) uses path quality — throughput and loss, not just level.
- **Blob fetch** — prefer the nearest provider (L1 → L2 → same-cluster → far); break ties by path quality.
- **Leader-per-LAN aggregation** — the bridge mechanism applied to fan-in (metrics, logs), cutting WAN chatter.
- **Replica placement** — spread replicas *across* clusters for failure-domain diversity (same-LAN replicas share a power strip); confidence gates promotion into a replica set.
- **Locality-aware gossip** — bias the overlay we already run; see next section.

These are consumers of the view, not part of this design; each gets its own note when real — except gossip, detailed below because the integration surface shapes v1/v2.

## iroh-gossip integration — locality as a property of the overlay

iroh-gossip *is* HyParView (membership: bounded `active_view`, default cap 5, over live connections; `passive_view`, cap 30, of known-but-unconnected peers, refreshed by shuffles) plus Plumtree (broadcast: eager/lazy push over the active view). So "locality-aware gossip" is not a new subsystem — it's biasing an overlay we already operate. Audit of our fork (`aster-rpc/iroh-gossip` @ 76492e4) splits the work cleanly:

### Usable today, no fork changes

- **Measurement feeder.** `Event::NeighborUp/NeighborDown` tell us exactly which peers we hold live QUIC connections to — each one is a free, continuous RTT/loss/throughput source for the topology view, and a local fast-path liveness hint (bridge *election* uses doc-record freshness instead — see bridge election). The view should subscribe to these events in v1.
- **Locality-aware seeding.** `JoinOptions.bootstrap` sets the initial active-view candidates per topic, and runtime `Command::JoinPeers` injects more. Once a node has cluster/ladder rankings, it feeds nearest-ranked peers as bootstrap/join candidates — HyParView then tends to keep them. This is the cheapest locality lever and needs zero protocol changes.
- **`PeerData` piggyback.** HyParView membership messages (Join/ForwardJoin/Shuffle) already carry an opaque per-peer `PeerData(Bytes)` blob, updatable via `UpdatePeerData`; the net layer currently packs `AddrInfo` into it. It could carry a compact locality hint (egress + ASN) so peers learned via shuffle arrive with a coarse position claim before any doc sync. With coordinates gone the payoff is smaller than originally sketched — candidate generation only, trust class **claim only** — so this may not earn its size budget (open question).
- **Config tuning.** `Builder::membership_config` / `broadcast_config` expose view capacities, shuffle intervals, random-walk TTLs, graft timeouts — enough to tune overlay shape without touching selection logic.

### Fork-level (v2+, we own the fork)

- **Biased passive→active promotion.** All selection in `proto/hyparview.rs` is uniformly random (`pick_random_without` in `refill_active_from_passive`, shuffle-response sampling). The minimal patch is a pluggable weight function at those call sites, defaulting to uniform, fed by the topology view's rankings. This is the X-BOT idea (Leitão et al. — same lineage as HyParView): topology-aware overlay optimisation driven by a cost oracle.
- **Reserved random slots — the partition guard.** A fully locality-greedy active view collapses into locality islands; HyParView's random walks are what keep the global overlay connected. The literature's answer (X-BOT keeps unbiased links; our earlier phrasing: "one long link per round") is to bias only k of the active-view slots and leave the rest uniformly random. Slot split is an open question below.
- **Latency-aware Plumtree (optional, later).** Plumtree's tree optimisation promotes lazy→eager by *hop count* (`optimization_threshold`), and graft timeouts are global constants rather than per-peer RTT. Once active views are locality-biased, hop count correlates better with latency anyway, so this may never be worth the complexity — measure first.

## Surface

Core (Rust) computes the view — it owns iroh path info, quinn stats, and the docs client. mDNS local discovery feeds L1 sightings into the same view. Exposed via the baseline catalog, adjacent to `aster.ops.Connections` in [../aster-baseline-services.md](../aster-baseline-services.md):

```
aster.net.Topology
  peers()                    → list<PeerView>
  peer(node_id)              → PeerView
  clusters()                 → list<ClusterView>
  myCluster()                → ClusterView
  separated(node_a, node_b)  → SeparationVerdict     # SEPARATED | CONNECTED | UNKNOWN, per coverage rule
```

```
ClusterView {
  members:   list<bytes(32)>   # sorted node ids; admitted ∧ live (fresh position) only
  bridge:    bytes(32)
  witnesses: list<bytes(32)>   # top witness_r by score, includes bridge
}
```

`PeerView` as defined in the Confidence section; both are `AsterType`s under the `aster.topo` payload root. Clusters carry no synthetic stable id — consumers identify a cluster by its member set (or by "the cluster containing node X"), because any synthetic id would either churn with membership or require the coordination this design avoids.

## Phasing

**v1 — read what iroh already knows.** L1 via verified private dial (+ mDNS when it lands), RTT/loss/jitter/throughput from live connections (quinn stats + gossip `NeighborUp` as the feeder). Ladder + path quality + confidence, with smoothing. Local view only; no new measurement machinery.

> **v1 shipped 2026-07-04 (Rust core + `aster` facade).** `core/src/topology.rs` sampler over `CoreMonitor`'s connection registry; `Node::topology()` in-process handle; opt-in `aster.net.Topology` baseline RPC service (`builtin_topology(true)`, gate with `topology_requires`). Gossip `NeighborUp` feeder not yet wired (the monitor hook already sees every connection, gossip included); mDNS L1 sightings pending the local-discovery feature. Python/FFI surfaces follow the binding wave.

**v2 (feature-flagged) — the shared doc + clusters.** Topology namespace, `NetworkPosition` + `RttEdge` records per the normative schema, corroborated edge rules, threshold-component clustering, witness sets + maintenance probing, bridge election, coverage rule, join flow, prefix-hash candidate matching. Gossip levers that need no fork changes: locality-aware bootstrap/`JoinPeers`.

**v3 — consumers.** Cluster-scoped replication policies, fork-level gossip biasing (weighted promotion + reserved random slots), blob-fetch strategies, leader-per-LAN.

## Open questions (v3 / scale-out only)

1. **Default tuning.** The Defaults table is normative but chosen from rules of thumb; run an empirical pass against real deployments (relay fleet geography, office LANs) before declaring the numbers final.
2. **Chaining in practice.** Components are pinned; if real topologies chain-merge regions (or colluders exploit it), switch the linkage rule to complete-linkage — the escape hatch is documented, the trigger is evidence.
3. **Doc growth at scale.** O(N) positions + intra-cluster edges is fine to thousands of nodes; beyond that, shard the namespace by cluster/region.
4. **Namespace rotation mechanics.** How the root re-keys and redistributes after revocation — shared open question with [../aster-sealed-grants.md](../aster-sealed-grants.md) (its Q4); one rotation design should serve both.
5. **Active-view slot split.** How many of the (default 5) HyParView active slots get locality-biased vs left uniformly random? Too greedy partitions the overlay into locality islands; too conservative wastes the bias. Needs simulation (iroh-gossip ships a `sim` harness we can extend) before the fork change lands.
6. **`PeerData` size budget — and whether the piggyback survives at all.** Membership messages carry `PeerData` on every Join/ForwardJoin/Shuffle; without a coordinate to carry, the remaining hint (egress + ASN, claim-only) may not justify taxing the whole overlay. Prefix hashes stay doc-only regardless.
7. **Bridge exclusivity.** The deterministic bridge is eventually-agreed, not exclusive. Which consumers are fine with transient double-bridging (replication: yes) and which need a real coordination primitive on top?

### Decided (was open, now pinned)

- Derived views (ladder/cluster assignments) are **not published** — everyone derives identically from raw corroborated signals; publishing would add claim-vs-verified ambiguity for zero information gain.
- Salt distribution — derived from `namespace_id`, nothing to distribute.
- Witness/coverage policy, timing constants, bridge-election bytes, record schema/versioning — see the normative sections above.
