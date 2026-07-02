# Network Topology Self-Mapping — Locality-Aware Aster Swarms

> **Status: working idea, not a spec.** Signal inventory, locality ladder, and the shared-doc substrate are design direction; record schemas, thresholds, and key layouts are illustrative.

**Companion docs:**
- [../aster-baseline-services.md](../aster-baseline-services.md) — the topology surface would live in the `aster.*` baseline catalog (`aster.net.Topology`, adjacent to `aster.ops.Connections`)
- Local discovery plan (mDNS + `AddrFilter`) — mDNS sightings are a Tier-1 feeder for this design, not a standalone feature

---

## Why

Nodes that want to self-organise — cluster together, pick replication partners, build aggregation trees, prefer local peers for blob fetch — need to know what's *local*, what's *nearby*, and what's *far*. No coordinator should be required: every node computes its own neighborhood view from signals it can observe or verify, plus claims replicated from other nodes.

Three questions, in increasing distance:

1. Which nodes are on the same private network / LAN?
2. Which nodes are "nearby" — same site, metro, or provider region?
3. Which nodes are distant (another region/geography)?

## Signal inventory

Most raw signals already exist in the stack (iroh path info, relay selection, planned mDNS). The design work is the inference layer, not new measurement machinery.

| Signal | Proves | Cost | Trust class |
|---|---|---|---|
| Direct QUIC conn on a private addr | Same routable private network (handshake proves peer identity — unspoofable) | Free — iroh path info | **Verified** |
| mDNS sighting | Same L2/broadcast domain | Free — planned local-discovery feature | **Verified** |
| Same observed public egress IP | Same NAT/site | Free — relay-observed address | Semi-verified |
| Live QUIC RTT | Latency distance | Free on active connections | **Measured** |
| Vivaldi coordinate | *Estimated* RTT to nodes never contacted | Gossip + ~200 lines of math | Estimated (gameable) |
| Same home relay | Coarse region (we run the relay fleet, so relay choice ≈ region tag) | Free | Semi-verified |
| IP → ASN lookup | Provider / network operator | ~10MB embedded offline DB (iptoasn-style) | DB is real, IP is claimed |
| Claimed interface/prefix list | Candidate same-LAN peers | Replicated record | **Claim only** |

**Core rule: claims generate candidates; verified/measured signals make decisions.** Anything that drives data placement or authority (leader-per-LAN, replica set membership) must rest on verified or measured signals. A malicious node can claim any subnet and shade its Vivaldi coordinate; it cannot fake a completed QUIC handshake on a private path, and it cannot make *your* measured RTT smaller.

### Ruled out

- **True BGP path data.** AS-paths live in router RIBs; nodes can't observe them without external feeds (RouteViews / RIPE RIS / looking-glass APIs), and AS-path length correlates poorly with latency anyway. The cheap proxy is ASN identity + RTT.
- **Traceroute-style probing.** Raw ICMP needs privileges, breaks on Windows/containers, adds little over RTT.
- **Raw private-IP comparison as a conclusion.** RFC1918 space is reused everywhere; `10.0.1.5/24` on two nodes proves nothing. Same-prefix match is a *candidate generator* whose confirmation is a verified private dial.

## The locality ladder

Each level derives from named signals; a node assigns every known peer the highest level it can justify:

```
L0 same-host    loopback / identical observed addrs
L1 same-lan     verified private dial  ∨  mDNS sighting
L2 same-site    same public egress IP, no private path (e.g. separate VLANs, one office)
L3 same-region  RTT < threshold  ∧  (same home relay ∨ same ASN)
L4 far          everything else
```

RTT bands (rules of thumb, to be tuned): <1ms same host/rack, <2ms LAN/DC, <15ms metro, <50ms region.

Levels must be **sticky** — hysteresis on minutes scale, smoothed RTT (QUIC's own smoothed-RTT estimator is the right input), and cluster-membership decisions consume the smoothed view only. Otherwise DHCP churn and RTT jitter make the hierarchy flap.

## Vivaldi coordinates

For distance between nodes that have **never connected**, use Vivaldi network coordinates (the Serf/Consul approach): each node maintains a point in a low-dimensional latency space, updated cheaply from RTTs it measures anyway during normal traffic, and publishes its coordinate. Any node estimates RTT between any two coordinates without probing — the estimate is good enough for *ranking* (who is nearest?), which is all the hierarchy needs; decisions that matter get confirmed by an actual dial.

Vivaldi is also what keeps the shared map **O(N)**: nodes publish one coordinate each instead of O(N²) raw RTT edges.

## Shared topology doc — the replication substrate

Topology knowledge replicates through a dedicated **iroh-docs namespace**. The root node provisions the namespace and propagates the namespace secret to admitted nodes via **sealed grants** — the per-recipient HPKE capability-distribution mechanism generalized from portal-sync, now implemented in `aster::grants` (see [../aster-sealed-grants.md](../aster-sealed-grants.md)). Nodes write their own observations; the CRDT store replicates them swarm-wide with no coordinator and no requirement that nodes be online simultaneously.

**Attribution survives the shared secret.** iroh-docs entries are signed by both the namespace key *and* the per-node author key — a shared write secret does not mean anonymous writes. Every record stays attributable to the node that wrote it, which is what keeps the claims-vs-verified split enforceable: the doc holds claims; each reader verifies its own edges.

### Record schema (illustrative)

Each node writes, under its own author key:

```
topo/<node_id>/position     → NetworkPosition (signed, self-describing)
```

```
NetworkPosition {
  node_id:          String        # hex public key
  observed_public:  String        # relay-observed public endpoint
  asn:              u32           # 0 = unknown
  home_relay:       String        # relay URL
  vivaldi:          Coord         # low-dim coordinate + error estimate
  prefix_hashes:    Vec<String>   # salted hashes of (private prefix, netmask) — see Privacy
  updated_unix_ms:  u64
}
```

Optionally a small set of **verified-edge attestations** — "I completed a private-path dial to P" — capped at top-K nearest peers so the doc stays O(N):

```
topo/<node_id>/lan/<peer_id> → { level: L1, verified_unix_ms }
```

Records carry timestamps; readers treat stale records (e.g. >1h) as decayed. iroh-docs keeps latest-per-(author, key), so refresh is a plain overwrite.

### Join flow

A node joining the swarm:

1. Gets admitted; receives the namespace secret; syncs the topology doc.
2. Reads all `position` records → has every peer's coordinate, relay, ASN, prefix hashes *before probing anything* (cold-start solved).
3. Matches its own private prefixes against published `prefix_hashes` → L1 **candidates**; confirms each by private-path dial (cheap, and the QUIC handshake verifies identity).
4. Probes 3–5 peers (relay-suggested or nearest-by-claimed-coordinate) to place its own Vivaldi coordinate.
5. Ranks the entire swarm by estimated RTT; assigns ladder levels; writes its own `position` record.

From there the node keeps its coordinate and record fresh from normal traffic — no dedicated probe traffic beyond the join burst.

### Trust & revocation

- **Readers filter by admission.** Records from authors that are not currently admitted are ignored at read time, regardless of what's in the doc.
- **Shared secret can't be revoked per-node.** A revoked node still holds the namespace secret; admission gating stops peers syncing *with* it, but records it wrote earlier persist until overwritten. On revocation events the root rotates the namespace and redistributes — acceptable for an internal swarm; noted as a known limitation.
- **Poisoning bounds.** A lying node can corrupt only what claims can influence: candidate generation and estimated rankings. It cannot manufacture verified L1 edges to peers it can't reach, and every consequential decision re-verifies by dialing.

## Privacy

Publishing raw internal prefixes leaks site topology to the whole swarm. Instead publish **salted hashes of (prefix, netmask)**: nodes on the same subnet share the input and still match; other nodes learn nothing about the address plan. Position records live in an admission-gated namespace, never exposed to arbitrary dialers.

## Consumers — what the hierarchy does with the ladder

The point of the tiers is to drive self-organisation:

- **Locality-aware gossip fan-out** — prefer L1/L2 peers for first-hop, one long link per round.
- **Blob fetch** — prefer the nearest provider (L1 → L2 → L3) before pulling cross-region.
- **Leader-per-LAN aggregation** — elect one L1 representative per site for fan-in (metrics, logs), cutting WAN chatter.
- **Replica placement** — spread replicas *across* ladder levels for failure-domain diversity (same-LAN replicas share a power strip).

These are consumers of the view, not part of this design; each gets its own note when real.

## Surface

- Core (Rust) computes the view — it owns iroh path info, relay state, RTT, and the docs client.
- Exposed via the baseline catalog: `aster.net.Topology` (query the local node's proximity view / ladder assignments), sitting alongside `aster.ops.Connections` in [aster-baseline-services.md](aster-baseline-services.md).
- mDNS local discovery feeds L1 sightings into the same view.

## Phasing

**v1 — read what iroh already knows.** L1 via verified private dial (+ mDNS when it lands), RTT bands from live connections, relay affinity for region. Ladder + smoothing. No new measurement machinery.

**v2 — the shared doc + coordinates.** Topology namespace, `NetworkPosition` records, Vivaldi coordinates, join flow, prefix-hash candidate matching, ASN DB.

**v3 — consumers.** Locality-aware gossip/blob strategies, leader-per-LAN, replica placement policies.

## Open questions

1. **Coordinate dimensionality + parameters.** Vivaldi with 4–8 dims + height is the usual choice; needs an empirical pass on our RTT distributions.
2. **Ladder thresholds.** The RTT bands are folklore numbers; tune against the relay fleet's real geography.
3. **Salt distribution for prefix hashes.** Per-swarm salt in the topology doc itself is simplest; per-epoch rotation would limit long-term correlation.
4. **Record TTL / decay policy.** 1h staleness is a guess; balance churn detection against write traffic.
5. **Doc growth at scale.** O(N) positions + O(K·N) edge attestations is fine to thousands of nodes; beyond that, consider sharding the namespace by region.
6. **Namespace rotation mechanics.** How the root re-keys and redistributes after revocation — shared open question with [../aster-sealed-grants.md](../aster-sealed-grants.md) (its Q4); one rotation design should serve both.
7. **Should ladder assignments themselves be published?** Publishing derived views (not just raw signals) lets small nodes freeload on inference, but multiplies claim-vs-verified ambiguity. Leaning no for v2.
