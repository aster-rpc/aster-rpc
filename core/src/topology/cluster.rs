//! Topology v2 cluster derivation — pure functions over validated records.
//!
//! Implements the normative rules of the design doc: vertices are admitted ∧
//! live authors; edges require corroboration (both halves fresh, held ≥
//! `min_hold`, `rtt_us ≤ hyst_exit`, max of the two sides); clusters are the
//! connected components; bridge/witnesses are the top scores of
//! `blake3(namespace_id ‖ "aster.topo.bridge.v1" ‖ node_id)`; `separated`
//! needs corroborated far pairs under a coverage rule. Because every node
//! runs this same pure function over the same replicated inputs, all nodes
//! converge on the same partition — agreement without consensus.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use super::records::{self, NetworkPosition, RttEdge, TopoKey};

/// Cluster-edge RTT bound used by readers (`hyst_exit` in the design doc's
/// defaults table): an edge half qualifies as near only at or below this,
/// and counts as far evidence only at or above it.
pub const CLUSTER_RTT_EXIT_US: i32 = 18_000;

/// Reader-side timing/coverage knobs. Defaults are the design doc's
/// normative table; tests inject shorter values.
#[derive(Clone, Debug)]
pub struct TopoConfig {
    /// Continuous hold below the enter threshold before an edge enters the
    /// graph.
    pub min_hold: Duration,
    /// A member is live iff its position record is fresher than this.
    pub liveness_ttl: Duration,
    /// An edge half is ignored if `measured_unix_ms` is older than this.
    pub edge_ttl: Duration,
    /// Records timestamped beyond now + this are dropped.
    pub max_skew: Duration,
    /// Distinct corroborated far pairs required for `Separated` (clamped
    /// to |X|·|Y| for small clusters).
    pub separation_coverage: usize,
    /// Witness-set size per cluster (clamped to the member count).
    pub witness_r: usize,
}

impl Default for TopoConfig {
    fn default() -> Self {
        Self {
            min_hold: Duration::from_secs(120),
            liveness_ttl: Duration::from_secs(300),
            edge_ttl: Duration::from_secs(900),
            max_skew: Duration::from_secs(30),
            separation_coverage: 2,
            witness_r: 3,
        }
    }
}

/// Validated, decoded records — the input to [`derive`]. Populated via
/// [`TopoRecords::insert_entry`], which enforces the reader's decode/drop
/// rules (key shape, attribution, future timestamps).
#[derive(Debug, Default)]
pub struct TopoRecords {
    /// author hex → latest position.
    pub positions: HashMap<String, NetworkPosition>,
    /// (author hex, peer hex) → author's half of the edge.
    pub edges: HashMap<(String, String), RttEdge>,
    /// Entries dropped by validation (decode failure, attribution
    /// mismatch, future-dated) — for observability, never fatal.
    pub dropped: u64,
}

impl TopoRecords {
    /// Apply one doc entry, enforcing the reader rules:
    /// - the key must parse as a known `topo/v1/…` shape (else ignored,
    ///   not counted — unknown versions are forward compatibility);
    /// - **attribution**: entry author == key node == embedded node_id;
    /// - **future timestamps**: any stamp beyond `now_ms + max_skew` drops
    ///   the record.
    pub fn insert_entry(
        &mut self,
        author_hex: &str,
        key: &[u8],
        value: &[u8],
        now_ms: u64,
        max_skew: Duration,
    ) {
        let Some(topo_key) = records::parse_key(key) else {
            return; // not a v1 key — ignore silently (forward compat)
        };
        if topo_key.node() != author_hex {
            self.dropped += 1; // forged path: author != key owner
            return;
        }
        let horizon = now_ms.saturating_add(max_skew.as_millis() as u64) as i64;
        match topo_key {
            TopoKey::Position { node } => match records::decode_position(value) {
                Ok(p) => {
                    if hex::encode(&p.node_id) != node || p.updated_unix_ms > horizon {
                        self.dropped += 1;
                        return;
                    }
                    self.positions.insert(node, p);
                }
                Err(_) => self.dropped += 1,
            },
            TopoKey::Rtt { node, peer } => match records::decode_rtt_edge(value) {
                Ok(e) => {
                    if e.measured_unix_ms > horizon || e.held_since_unix_ms > horizon {
                        self.dropped += 1;
                        return;
                    }
                    self.edges.insert((node, peer), e);
                }
                Err(_) => self.dropped += 1,
            },
            // LAN attestations don't participate in cluster derivation yet.
            TopoKey::Lan { .. } => {}
        }
    }
}

/// `separated()` outcome. `Unknown` is a real answer: absence of a
/// measurement is never treated as evidence of distance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreSeparationVerdict {
    Unknown,
    Connected,
    Separated,
}

/// One derived cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreClusterView {
    /// Sorted hex node ids; admitted ∧ live members only.
    pub members: Vec<String>,
    pub bridge: String,
    /// Top `witness_r` members by bridge score (includes the bridge).
    pub witnesses: Vec<String>,
}

/// The derived shared view: clusters plus the evidence needed to answer
/// `separated()`.
#[derive(Debug, Default)]
pub struct SharedView {
    pub clusters: Vec<CoreClusterView>,
    /// Entries dropped during validation (carried from [`TopoRecords`]).
    pub dropped: u64,
    /// node hex → index into `clusters`.
    cluster_of: HashMap<String, usize>,
    /// Canonically-ordered corroborated far pairs (both halves fresh, both
    /// ≥ exit threshold).
    far_pairs: HashSet<(String, String)>,
    separation_coverage: usize,
}

impl SharedView {
    pub fn cluster_of(&self, node_hex: &str) -> Option<&CoreClusterView> {
        self.cluster_of.get(node_hex).map(|i| &self.clusters[*i])
    }

    /// The design doc's coverage rule: `Separated` iff no qualifying edge
    /// joins the two clusters (they are distinct components) **and** at
    /// least `min(separation_coverage, |X|·|Y|)` distinct corroborated far
    /// pairs span them. Anything less is `Unknown`.
    pub fn separated(&self, a: &str, b: &str) -> CoreSeparationVerdict {
        let (Some(&ca), Some(&cb)) = (self.cluster_of.get(a), self.cluster_of.get(b)) else {
            return CoreSeparationVerdict::Unknown;
        };
        if ca == cb {
            return CoreSeparationVerdict::Connected;
        }
        let (xs, ys) = (&self.clusters[ca].members, &self.clusters[cb].members);
        let need = self.separation_coverage.min(xs.len() * ys.len());
        let mut have = 0usize;
        for x in xs {
            for y in ys {
                if self.far_pairs.contains(&ordered(x, y)) {
                    have += 1;
                    if have >= need {
                        return CoreSeparationVerdict::Separated;
                    }
                }
            }
        }
        CoreSeparationVerdict::Unknown
    }
}

fn ordered(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.into(), b.into())
    } else {
        (b.into(), a.into())
    }
}

/// Bridge/witness score — `blake3(namespace_id ‖ "aster.topo.bridge.v1" ‖
/// node_id)`, compared as 32 big-endian bytes (higher wins), tie-broken by
/// lower node id. Deliberately independent of cluster composition so
/// membership changes never reshuffle rankings.
fn bridge_score(namespace_id: &[u8; 32], node_hex: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(namespace_id);
    h.update(b"aster.topo.bridge.v1");
    h.update(&hex::decode(node_hex).expect("validated node hex"));
    *h.finalize().as_bytes()
}

/// Derive the shared view. Pure: same records + same config + same clock →
/// same output on every node.
pub fn derive(
    records: &TopoRecords,
    admitted: &dyn Fn(&str) -> bool,
    namespace_id: &[u8; 32],
    now_ms: u64,
    cfg: &TopoConfig,
) -> SharedView {
    let now = now_ms as i64;
    let liveness_ms = cfg.liveness_ttl.as_millis() as i64;
    let edge_ms = cfg.edge_ttl.as_millis() as i64;
    let min_hold_ms = cfg.min_hold.as_millis() as i64;

    // Vertices: admitted authors with a live position record. BTreeMap for
    // deterministic iteration.
    let vertices: BTreeMap<&str, ()> = records
        .positions
        .iter()
        .filter(|(node, p)| now - p.updated_unix_ms <= liveness_ms && admitted(node))
        .map(|(node, _)| (node.as_str(), ()))
        .collect();

    let is_vertex = |n: &str| vertices.contains_key(n);
    let half_fresh = |e: &RttEdge| now - e.measured_unix_ms <= edge_ms;
    let half_near = |e: &RttEdge| {
        half_fresh(e)
            && e.held_since_unix_ms != 0
            && now - e.held_since_unix_ms >= min_hold_ms
            && e.rtt_us <= CLUSTER_RTT_EXIT_US
    };
    let half_far = |e: &RttEdge| half_fresh(e) && e.rtt_us >= CLUSTER_RTT_EXIT_US;

    // Qualified (near) edges + corroborated far pairs, canonical order.
    let mut near_pairs: Vec<(&str, &str)> = Vec::new();
    let mut far_pairs: HashSet<(String, String)> = HashSet::new();
    for ((a, b), e_ab) in &records.edges {
        if a >= b {
            continue; // visit each unordered pair once, from its low side
        }
        if !is_vertex(a) || !is_vertex(b) {
            continue;
        }
        let Some(e_ba) = records.edges.get(&(b.clone(), a.clone())) else {
            continue; // one-sided record is a candidate, not an edge
        };
        if half_near(e_ab) && half_near(e_ba) {
            near_pairs.push((a, b));
        } else if half_far(e_ab) && half_far(e_ba) {
            far_pairs.insert((a.clone(), b.clone()));
        }
    }

    // Connected components over (vertices, near_pairs) — union-find.
    let index: HashMap<&str, usize> = vertices.keys().enumerate().map(|(i, n)| (*n, i)).collect();
    let mut parent: Vec<usize> = (0..vertices.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for (a, b) in &near_pairs {
        let (ra, rb) = (find(&mut parent, index[a]), find(&mut parent, index[b]));
        if ra != rb {
            parent[ra.max(rb)] = ra.min(rb);
        }
    }

    let mut components: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for node in vertices.keys() {
        let root = find(&mut parent, index[node]);
        components.entry(root).or_default().push((*node).into());
    }

    let mut clusters = Vec::with_capacity(components.len());
    let mut cluster_of = HashMap::new();
    for (_, mut members) in components {
        members.sort();
        // Rank by (score desc, node asc); scores are composition-independent.
        let mut ranked: Vec<(String, [u8; 32])> = members
            .iter()
            .map(|m| (m.clone(), bridge_score(namespace_id, m)))
            .collect();
        ranked.sort_by(|(na, sa), (nb, sb)| sb.cmp(sa).then_with(|| na.cmp(nb)));
        let bridge = ranked[0].0.clone();
        let witnesses: Vec<String> = ranked
            .iter()
            .take(cfg.witness_r.min(ranked.len()))
            .map(|(n, _)| n.clone())
            .collect();
        let idx = clusters.len();
        for m in &members {
            cluster_of.insert(m.clone(), idx);
        }
        clusters.push(CoreClusterView {
            members,
            bridge,
            witnesses,
        });
    }

    SharedView {
        clusters,
        dropped: records.dropped,
        cluster_of,
        far_pairs,
        separation_coverage: cfg.separation_coverage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: [u8; 32] = [42u8; 32];
    const NOW: u64 = 1_800_000_000_000;

    fn id(b: u8) -> String {
        hex::encode([b; 32])
    }

    fn all(_: &str) -> bool {
        true
    }

    /// Short-timing config so synthetic ages stay readable.
    fn cfg() -> TopoConfig {
        TopoConfig::default()
    }

    fn pos(recs: &mut TopoRecords, node: &str, age_ms: i64) {
        recs.positions.insert(
            node.into(),
            NetworkPosition {
                node_id: hex::decode(node).unwrap(),
                updated_unix_ms: NOW as i64 - age_ms,
                ..Default::default()
            },
        );
    }

    /// Insert both halves of a corroborated edge with the given RTT, held
    /// long enough to qualify when `near` is expected.
    fn edge_both(recs: &mut TopoRecords, a: &str, b: &str, rtt_us: i32) {
        for (x, y) in [(a, b), (b, a)] {
            recs.edges.insert(
                (x.into(), y.into()),
                RttEdge {
                    rtt_us,
                    samples: 100,
                    held_since_unix_ms: if rtt_us <= CLUSTER_RTT_EXIT_US {
                        NOW as i64 - 600_000 // held 10 min > min_hold 2 min
                    } else {
                        0
                    },
                    measured_unix_ms: NOW as i64 - 10_000,
                },
            );
        }
    }

    fn three_plus_three() -> TopoRecords {
        // Cluster X: 0xa1,0xa2,0xa3; cluster Y: 0xb1,0xb2,0xb3.
        let mut r = TopoRecords::default();
        for n in [0xa1, 0xa2, 0xa3, 0xb1, 0xb2, 0xb3] {
            pos(&mut r, &id(n), 1_000);
        }
        for (x, y) in [(0xa1, 0xa2), (0xa2, 0xa3), (0xa1, 0xa3)] {
            edge_both(&mut r, &id(x), &id(y), 2_000);
        }
        for (x, y) in [(0xb1, 0xb2), (0xb2, 0xb3), (0xb1, 0xb3)] {
            edge_both(&mut r, &id(x), &id(y), 3_000);
        }
        // Cross-region far measurements: bridges/witness probes.
        edge_both(&mut r, &id(0xa1), &id(0xb1), 70_000);
        edge_both(&mut r, &id(0xa2), &id(0xb2), 72_000);
        r
    }

    #[test]
    fn three_plus_three_derives_two_clusters_with_bridges() {
        let r = three_plus_three();
        let view = derive(&r, &all, &NS, NOW, &cfg());
        assert_eq!(view.clusters.len(), 2);
        for c in &view.clusters {
            assert_eq!(c.members.len(), 3);
            assert!(c.members.contains(&c.bridge));
            assert_eq!(c.witnesses.len(), 3); // witness_r = 3 = |members|
            assert_eq!(c.witnesses[0], c.bridge, "bridge is rank 1 witness");
        }
        // Same-cluster → Connected; cross with 2 far pairs → Separated.
        assert_eq!(
            view.separated(&id(0xa1), &id(0xa2)),
            CoreSeparationVerdict::Connected
        );
        assert_eq!(
            view.separated(&id(0xa1), &id(0xb3)),
            CoreSeparationVerdict::Separated
        );
        assert_eq!(
            view.separated(&id(0xa1), &id(0xff)),
            CoreSeparationVerdict::Unknown,
            "unknown node → Unknown"
        );
    }

    #[test]
    fn derivation_is_deterministic() {
        let r = three_plus_three();
        let v1 = derive(&r, &all, &NS, NOW, &cfg());
        let v2 = derive(&r, &all, &NS, NOW, &cfg());
        assert_eq!(v1.clusters, v2.clusters);
    }

    #[test]
    fn one_sided_edge_is_no_edge() {
        let mut r = TopoRecords::default();
        pos(&mut r, &id(1), 0);
        pos(&mut r, &id(2), 0);
        r.edges.insert(
            (id(1), id(2)),
            RttEdge {
                rtt_us: 1_000,
                samples: 10,
                held_since_unix_ms: NOW as i64 - 600_000,
                measured_unix_ms: NOW as i64 - 1_000,
            },
        );
        let view = derive(&r, &all, &NS, NOW, &cfg());
        assert_eq!(view.clusters.len(), 2, "a liar cannot manufacture B's half");
    }

    #[test]
    fn stale_position_drops_vertex_within_liveness_ttl() {
        let mut r = TopoRecords::default();
        pos(&mut r, &id(1), 0);
        pos(&mut r, &id(2), 301_000); // > liveness_ttl (300s)
        edge_both(&mut r, &id(1), &id(2), 2_000);
        let view = derive(&r, &all, &NS, NOW, &cfg());
        assert_eq!(view.clusters.len(), 1);
        assert_eq!(view.clusters[0].members, vec![id(1)]);
    }

    #[test]
    fn hold_gating_blocks_unheld_edges() {
        let mut r = TopoRecords::default();
        pos(&mut r, &id(1), 0);
        pos(&mut r, &id(2), 0);
        edge_both(&mut r, &id(1), &id(2), 2_000);
        // Rewind one half's held_since to "just now" — under min_hold.
        r.edges.get_mut(&(id(1), id(2))).unwrap().held_since_unix_ms = NOW as i64 - 1_000;
        let view = derive(&r, &all, &NS, NOW, &cfg());
        assert_eq!(view.clusters.len(), 2, "held < min_hold on one side");
    }

    #[test]
    fn admission_filter_removes_vertex_and_its_edges() {
        let r = three_plus_three();
        let banned = id(0xa1);
        let admitted = move |n: &str| n != banned;
        let view = derive(&r, &admitted, &NS, NOW, &cfg());
        let sizes: Vec<usize> = view.clusters.iter().map(|c| c.members.len()).collect();
        assert!(sizes.contains(&2) && sizes.contains(&3));
        assert!(view.cluster_of(&id(0xa1)).is_none());
    }

    #[test]
    fn bridge_stable_under_non_bridge_membership_change() {
        let r = three_plus_three();
        let view = derive(&r, &all, &NS, NOW, &cfg());
        let cluster_x = view.cluster_of(&id(0xa1)).unwrap().clone();

        // Remove a non-bridge member of X via admission; bridge must hold.
        let non_bridge = cluster_x
            .members
            .iter()
            .find(|m| **m != cluster_x.bridge)
            .unwrap()
            .clone();
        let admitted = move |n: &str| n != non_bridge;
        let view2 = derive(&r, &admitted, &NS, NOW, &cfg());
        let cluster_x2 = view2.cluster_of(&cluster_x.bridge).unwrap();
        assert_eq!(cluster_x2.bridge, cluster_x.bridge);
        assert_eq!(cluster_x2.members.len(), 2);
    }

    #[test]
    fn bridge_fails_over_when_its_records_decay() {
        let mut r = three_plus_three();
        let view = derive(&r, &all, &NS, NOW, &cfg());
        let cluster_x = view.cluster_of(&id(0xa1)).unwrap().clone();
        let old_bridge = cluster_x.bridge.clone();
        let successor = cluster_x.witnesses[1].clone();

        // Bridge's position record goes stale → next rank promotes.
        pos(&mut r, &old_bridge, 400_000);
        let view2 = derive(&r, &all, &NS, NOW, &cfg());
        let cluster_x2 = view2.cluster_of(&successor).unwrap();
        assert_eq!(cluster_x2.bridge, successor);
        assert!(!cluster_x2.members.contains(&old_bridge));
    }

    #[test]
    fn separated_needs_coverage_and_clamps_for_singletons() {
        // Two singletons with one corroborated far pair: coverage clamps
        // to |X|·|Y| = 1 → Separated.
        let mut r = TopoRecords::default();
        pos(&mut r, &id(1), 0);
        pos(&mut r, &id(2), 0);
        edge_both(&mut r, &id(1), &id(2), 70_000);
        let view = derive(&r, &all, &NS, NOW, &cfg());
        assert_eq!(
            view.separated(&id(1), &id(2)),
            CoreSeparationVerdict::Separated
        );

        // Two 2-clusters with only ONE far pair: need min(2, 4) = 2 → Unknown.
        let mut r = TopoRecords::default();
        for n in 1..=4u8 {
            pos(&mut r, &id(n), 0);
        }
        edge_both(&mut r, &id(1), &id(2), 2_000);
        edge_both(&mut r, &id(3), &id(4), 2_000);
        edge_both(&mut r, &id(1), &id(3), 70_000);
        let view = derive(&r, &all, &NS, NOW, &cfg());
        assert_eq!(
            view.separated(&id(1), &id(4)),
            CoreSeparationVerdict::Unknown,
            "one far pair < coverage 2"
        );

        // Add the second far pair → Separated.
        let mut r2 = TopoRecords::default();
        for n in 1..=4u8 {
            pos(&mut r2, &id(n), 0);
        }
        edge_both(&mut r2, &id(1), &id(2), 2_000);
        edge_both(&mut r2, &id(3), &id(4), 2_000);
        edge_both(&mut r2, &id(1), &id(3), 70_000);
        edge_both(&mut r2, &id(2), &id(4), 70_000);
        let view = derive(&r2, &all, &NS, NOW, &cfg());
        assert_eq!(
            view.separated(&id(1), &id(4)),
            CoreSeparationVerdict::Separated
        );
    }

    #[test]
    fn insert_entry_enforces_reader_rules() {
        let mut r = TopoRecords::default();
        let a = id(0xa1);
        let b = id(0xb1);
        let now = NOW;
        let skew = Duration::from_secs(30);

        let good = records::NetworkPosition {
            node_id: hex::decode(&a).unwrap(),
            updated_unix_ms: now as i64 - 1_000,
            ..Default::default()
        };

        // Accepted: author == key node == embedded id.
        r.insert_entry(
            &a,
            &records::position_key(&a),
            &records::encode_position(&good),
            now,
            skew,
        );
        assert_eq!(r.positions.len(), 1);
        assert_eq!(r.dropped, 0);

        // Forged path: A writes under B's key → dropped.
        r.insert_entry(
            &a,
            &records::position_key(&b),
            &records::encode_position(&good),
            now,
            skew,
        );
        assert_eq!(r.dropped, 1);

        // Embedded id mismatch: key/author = A but record claims B.
        let claims_b = records::NetworkPosition {
            node_id: hex::decode(&b).unwrap(),
            updated_unix_ms: now as i64 - 1_000,
            ..Default::default()
        };
        r.insert_entry(
            &a,
            &records::position_key(&a),
            &records::encode_position(&claims_b),
            now,
            skew,
        );
        assert_eq!(r.dropped, 2);

        // Future-dated beyond skew → dropped.
        let future = records::NetworkPosition {
            node_id: hex::decode(&a).unwrap(),
            updated_unix_ms: now as i64 + 60_000,
            ..Default::default()
        };
        r.insert_entry(
            &a,
            &records::position_key(&a),
            &records::encode_position(&future),
            now,
            skew,
        );
        assert_eq!(r.dropped, 3);

        // Garbage value → dropped; unknown key version → silently ignored.
        r.insert_entry(&a, &records::position_key(&a), b"junk", now, skew);
        assert_eq!(r.dropped, 4);
        r.insert_entry(
            &a,
            format!("topo/v9/{a}/position").as_bytes(),
            b"whatever",
            now,
            skew,
        );
        assert_eq!(r.dropped, 4, "unknown version is not an error");
    }
}
