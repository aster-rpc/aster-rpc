//! §S4-G: the §7.7 placement gate.
//!
//! Two halves. The **pure half** drives the planner directly: permuted
//! inputs, every forbidden substitution the `DECIDED S4-PLACEMENT`
//! regression list names (producer-as-eligible, Unknown-as-Separated,
//! bridge score as voter score, jitter-driven rebalance, two automatic
//! replacements, dropped keep-current rule), the corridor shape, and the
//! group-specific tie-break. Topologies are deterministic
//! [`TopologySnapshot`]s assembled from explicit parts — the same view type
//! and the same coverage rule production derives, without a swarm.
//!
//! The **manager half** runs `CoordinationPlacementManager` against a real
//! three-voter group over real RPC: two managers racing one revision commit
//! exactly one intent; a restart clears an uncommitted absence timer and
//! thereby delays (never shortens) the replacement grace; a policy change
//! between plan and submission discards the plan at the recheck; a
//! journalled membership request fsynced by a previous life resolves before
//! any new plan; and a group without quorum gets a report and no command.

#![cfg(feature = "coordination-testing")]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::coordination::client::manager::{
    CoordinationPlacementManager, PlacementPolicySource, PlacementProvisioner, ProvisionRequest,
    TopologySource,
};
use aster::coordination::client::placement::{
    plan, PlacementCandidate, PlacementInput, PlacementPolicy, PlacementRecommendation,
};
use aster::coordination::client::CoordinationClient;
use aster::coordination::contract::MemberSeedV1;
use aster::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;
use aster::topology::TopologySnapshot;
use aster::{AsterConfig, Node, PublicKey, RelayMode, SecretKey};
use aster_transport_core::{CoreClusterView, CoreTopoSharedView};

const PATIENCE: Duration = Duration::from_secs(60);

async fn within<T, F: std::future::Future<Output = T>>(what: &str, f: F) -> T {
    match tokio::time::timeout(PATIENCE, f).await {
        Ok(value) => value,
        Err(_) => panic!("{what}: no answer within {PATIENCE:?}"),
    }
}

// ── Pure-half fixtures ──────────────────────────────────────────────────────

fn node(tag: u8) -> CoordinationNodeId {
    CoordinationNodeId::from_slice(&[tag; 32]).expect("32 bytes")
}

fn group() -> CoordinationGroupId {
    CoordinationGroupId::from_slice(&[0xA7; 32]).expect("32 bytes")
}

/// A deterministic snapshot: clusters of nodes, with every cross-cluster
/// pair in `far` corroborated (coverage 1, so one far pair proves
/// separation and none leaves it `Unknown`).
fn snapshot(
    clusters: &[&[CoordinationNodeId]],
    far: &[(CoordinationNodeId, CoordinationNodeId)],
) -> TopologySnapshot {
    let views: Vec<CoreClusterView> = clusters
        .iter()
        .map(|members| {
            let mut hexes: Vec<String> = members.iter().map(|n| n.to_hex()).collect();
            hexes.sort();
            CoreClusterView {
                bridge: hexes.first().cloned().unwrap_or_default(),
                witnesses: Vec::new(),
                members: hexes,
            }
        })
        .collect();
    let pairs: Vec<(String, String)> = far.iter().map(|(a, b)| (a.to_hex(), b.to_hex())).collect();
    TopologySnapshot::from_view(Arc::new(CoreTopoSharedView::from_parts(views, pairs, 1)))
}

/// The pure half's authority: a real signing key, so every fixture permit
/// carries a genuine signature the planner's full `verify_join_permit` —
/// the same one apply runs — accepts, and a tampered one fails.
const PURE_AUTHORITY_SECRET: [u8; 32] = [0xA9; 32];
const PURE_AUTHORITY_DOMAIN: &str = "placement.test";
const PURE_GENESIS_HASH: [u8; 32] = [0x55; 32];

fn pure_authority_key() -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(&PURE_AUTHORITY_SECRET)
        .verifying_key()
        .to_bytes()
}

/// A fully signed permit for `id` under the pure authority, binding the pure
/// group, genesis and domain. `group_id`/`genesis_hash` are parameters so
/// the misbinding gates can mint a canonical permit for the wrong world.
fn signed_permit(id: CoordinationNodeId, group_id: [u8; 32], genesis_hash: [u8; 32]) -> Vec<u8> {
    use aster::coordination::contract::payload_fory;
    use aster::coordination::model::authority::{join_signing_bytes, signed_document_from_parts};
    use aster::coordination::raft::{JoinPermitV1, NodeIdV1};
    use ed25519_dalek::Signer;
    let permit = JoinPermitV1 {
        format_version: 1,
        group_id: group_id.to_vec(),
        authority_domain: PURE_AUTHORITY_DOMAIN.to_string(),
        genesis_hash: genesis_hash.to_vec(),
        candidate_node_id: id.as_bytes().to_vec(),
        candidate_ticket: aster::coordination::model::authority::canonical_candidate_ticket(
            id.as_bytes(),
        )
        .expect("canonical"),
        bootstrap_senders: vec![NodeIdV1 {
            node_id: vec![0x01; 32],
        }],
        join_nonce: vec![0x66; 32],
    };
    let document_bytes = payload_fory::<JoinPermitV1>()
        .serialize(&permit)
        .expect("the permit encodes");
    let signature = ed25519_dalek::SigningKey::from_bytes(&PURE_AUTHORITY_SECRET)
        .sign(&join_signing_bytes(&document_bytes));
    signed_document_from_parts(document_bytes, &pure_authority_key(), &signature.to_bytes())
        .expect("the envelope encodes")
}

/// A seed whose ticket is canonical and whose permit fully verifies under
/// the pure authority — signature, group, genesis, domain, candidate and
/// ticket bindings alike.
fn seeded(id: CoordinationNodeId) -> PlacementCandidate {
    PlacementCandidate {
        node_id: id,
        member_seed: Some(MemberSeedV1 {
            node_id: id.as_bytes().to_vec(),
            ticket: aster::coordination::model::authority::canonical_candidate_ticket(
                id.as_bytes(),
            )
            .expect("canonical"),
            signed_join_permit: Some(signed_permit(id, *group().as_bytes(), PURE_GENESIS_HASH)),
        }),
    }
}

fn unseeded(id: CoordinationNodeId) -> PlacementCandidate {
    PlacementCandidate {
        node_id: id,
        member_seed: None,
    }
}

fn policy(
    eligible: Vec<PlacementCandidate>,
    target: u8,
    minimum_pairs: u8,
    rebalance: bool,
) -> PlacementPolicy {
    PlacementPolicy {
        revision: 1,
        digest: [0xD1; 32],
        authority_public_key: pure_authority_key(),
        authority_domain: PURE_AUTHORITY_DOMAIN.to_string(),
        target_voters: target,
        target_retained_learners: 0,
        eligible: eligible.into_iter().map(|c| (c.node_id, c)).collect(),
        pinned_voters: BTreeSet::new(),
        excluded: BTreeSet::new(),
        minimum_separated_voter_pairs: minimum_pairs,
        replacement_grace: Duration::from_secs(600),
        allow_healthy_rebalance: rebalance,
    }
}

const PROFILE_PROD: i32 = 1;

fn input(
    current: &[CoordinationNodeId],
    policy: PlacementPolicy,
    topology: TopologySnapshot,
) -> PlacementInput {
    PlacementInput {
        group_id: group(),
        genesis_hash: PURE_GENESIS_HASH,
        deployment_profile_code: PROFILE_PROD,
        membership_log_id: None,
        current_voters: current.iter().copied().collect(),
        current_learners: BTreeSet::new(),
        active_membership_intent: false,
        grace_crossed_members: BTreeSet::new(),
        policy,
        topology,
    }
}

// ── The pure half ───────────────────────────────────────────────────────────

/// **Input order is irrelevant.** The same policy assembled in two insertion
/// orders — maps, pins, exclusions and the eligible list itself — yields the
/// same recommendation, byte for byte.
#[test]
fn the_planner_is_input_order_independent() {
    let (a, b, c, d, e) = (node(1), node(2), node(3), node(4), node(5));
    let topo = || {
        snapshot(
            &[&[a, b], &[c, d], &[e]],
            &[(a, c), (a, e), (c, e), (b, d), (b, e), (d, e)],
        )
    };
    let forward = vec![seeded(a), seeded(b), seeded(c), seeded(d), seeded(e)];
    let backward = vec![seeded(e), seeded(d), seeded(c), seeded(b), seeded(a)];
    let mut policy_fwd = policy(forward, 3, 1, true);
    let mut policy_bwd = policy(backward, 3, 1, true);
    for p in [&mut policy_fwd, &mut policy_bwd] {
        p.excluded.insert(node(9));
    }
    policy_fwd.pinned_voters.insert(a);
    policy_bwd.pinned_voters.insert(a);

    let one = plan(&input(&[a, b, c], policy_fwd, topo()));
    let two = plan(&input(&[a, b, c], policy_bwd, topo()));
    assert_eq!(
        format!("{one:?}"),
        format!("{two:?}"),
        "one typed input, one answer"
    );
}

/// **Producer admission is not eligibility.** A node that is a live topology
/// vertex but absent from the eligible map is never selected, however good
/// its separation evidence.
#[test]
fn an_admitted_but_ineligible_producer_is_never_selected() {
    let (a, b, c, outsider) = (node(1), node(2), node(3), node(7));
    // The outsider sits alone in a beautifully separated cluster.
    let topo = snapshot(
        &[&[a, b, c], &[outsider]],
        &[(a, outsider), (b, outsider), (c, outsider)],
    );
    let recommendation = plan(&input(
        &[a, b, c],
        policy(vec![seeded(a), seeded(b), seeded(c)], 3, 0, true),
        topo,
    ));
    match recommendation {
        PlacementRecommendation::Stable { .. } => {}
        other => panic!("the ineligible outsider moved the plan: {other:?}"),
    }
}

/// **`Unknown` is never `Separated`.** Distinct clusters with no
/// corroborated far pairs reach cardinality but cannot satisfy a positive
/// separated-pair minimum: the answer is `InsufficientEvidence`, naming the
/// shortfall, not a silently weakened policy.
#[test]
fn unknown_evidence_never_counts_as_separation() {
    let (a, b, c) = (node(1), node(2), node(3));
    // Three singleton clusters, zero far pairs: every pair is Unknown.
    let topo = snapshot(&[&[a], &[b], &[c]], &[]);
    let recommendation = plan(&input(
        &[a, b, c],
        policy(vec![seeded(a), seeded(b), seeded(c)], 3, 1, true),
        topo,
    ));
    match recommendation {
        PlacementRecommendation::InsufficientEvidence { missing } => {
            assert!(missing.contains("separated pairs"), "{missing}");
        }
        other => panic!("Unknown was counted as separation: {other:?}"),
    }
}

/// **The bridge score is not a voter score.** Two snapshots identical but
/// for which member each cluster names as bridge produce identical
/// recommendations.
#[test]
fn changing_only_the_bridge_changes_no_voter() {
    let (a, b, c, d) = (node(1), node(2), node(3), node(4));
    let far = [(a, c), (a, d), (b, c), (b, d)];
    let with_bridge = |bridge_first: bool| {
        let mut left: Vec<String> = vec![a.to_hex(), b.to_hex()];
        left.sort();
        if !bridge_first {
            left.reverse();
        }
        let views = vec![
            CoreClusterView {
                bridge: left[0].clone(),
                witnesses: vec![left[0].clone()],
                members: {
                    let mut m = left.clone();
                    m.sort();
                    m
                },
            },
            CoreClusterView {
                bridge: c.to_hex(),
                witnesses: Vec::new(),
                members: {
                    let mut m = vec![c.to_hex(), d.to_hex()];
                    m.sort();
                    m
                },
            },
        ];
        let pairs: Vec<(String, String)> =
            far.iter().map(|(x, y)| (x.to_hex(), y.to_hex())).collect();
        TopologySnapshot::from_view(Arc::new(CoreTopoSharedView::from_parts(views, pairs, 1)))
    };
    let candidates = || vec![seeded(a), seeded(b), seeded(c), seeded(d)];
    let one = plan(&input(
        &[a, b, c],
        policy(candidates(), 3, 0, true),
        with_bridge(true),
    ));
    let two = plan(&input(
        &[a, b, c],
        policy(candidates(), 3, 0, true),
        with_bridge(false),
    ));
    assert_eq!(
        format!("{one:?}"),
        format!("{two:?}"),
        "bridge rank moved a voter"
    );
}

/// **Jitter cannot rebalance authority.** Cluster composition shuffles
/// between two observations, every current voter stays live and healthy,
/// and without `allow_healthy_rebalance` the answer both times keeps every
/// voter: Stable, no plan.
#[test]
fn cluster_jitter_retains_every_healthy_voter() {
    let (a, b, c, d) = (node(1), node(2), node(3), node(4));
    let candidates = || vec![seeded(a), seeded(b), seeded(c), seeded(d)];
    let before = snapshot(&[&[a, b], &[c, d]], &[(a, c), (b, d)]);
    let after = snapshot(&[&[a], &[b, c, d]], &[(a, b), (a, c), (a, d)]);
    for topo in [before, after] {
        match plan(&input(&[a, b, c], policy(candidates(), 3, 0, false), topo)) {
            PlacementRecommendation::Stable { .. } => {}
            other => panic!("jitter moved a healthy voter: {other:?}"),
        }
    }
}

/// **Meeting the minimum by moving a healthy voter is `Degraded`, not a
/// plan.** The evidence exists, policy does not authorize the move, and a
/// topology merge therefore lowers the report without churning authority.
#[test]
fn an_unauthorized_healthy_move_reports_degraded() {
    let (a1, a2, a3, b1, c1) = (node(1), node(2), node(3), node(4), node(5));
    let topo = snapshot(
        &[&[a1, a2, a3], &[b1], &[c1]],
        &[
            (a1, b1),
            (a1, c1),
            (a2, b1),
            (a2, c1),
            (a3, b1),
            (a3, c1),
            (b1, c1),
        ],
    );
    let recommendation = plan(&input(
        &[a1, a2, a3],
        policy(
            vec![seeded(a1), seeded(a2), seeded(a3), seeded(b1), seeded(c1)],
            3,
            1,
            false,
        ),
        topo,
    ));
    match recommendation {
        PlacementRecommendation::Degraded { why, .. } => {
            assert!(why.contains("allow_healthy_rebalance"), "{why}");
        }
        other => panic!("a healthy voter was moved without authorization: {other:?}"),
    }
}

/// **One automatic replacement per plan.** With two grace-crossed voters and
/// two live seeded candidates, the plan replaces exactly one and retains the
/// other crossed member for the next reconciliation.
#[test]
fn automatic_maintenance_replaces_at_most_one_voter() {
    let (v0, v1, v2, c1, c2) = (node(1), node(2), node(3), node(4), node(5));
    let topo = snapshot(&[&[v0, c1, c2]], &[]);
    let mut planning = input(
        &[v0, v1, v2],
        policy(
            vec![seeded(v0), seeded(v1), seeded(v2), seeded(c1), seeded(c2)],
            3,
            0,
            false,
        ),
        topo,
    );
    planning.grace_crossed_members = [v1, v2].into_iter().collect();
    match plan(&planning) {
        PlacementRecommendation::Plan(plan) => {
            let selected: BTreeSet<CoordinationNodeId> =
                plan.desired_voters.iter().copied().collect();
            let current: BTreeSet<CoordinationNodeId> = [v0, v1, v2].into_iter().collect();
            let new: Vec<_> = selected.difference(&current).collect();
            assert_eq!(new.len(), 1, "exactly one new voter: {selected:?}");
            assert!(selected.contains(&v0), "the healthy voter stays");
            assert_eq!(
                plan.member_seeds.len(),
                1,
                "one seed for the one new member"
            );
        }
        other => panic!("no single-replacement plan: {other:?}"),
    }
}

/// **An active intent yields `Converging`** — no second plan exists, whatever
/// the topology says.
#[test]
fn an_active_intent_suppresses_planning() {
    let (a, b, c) = (node(1), node(2), node(3));
    let mut planning = input(
        &[a, b, c],
        policy(vec![seeded(a), seeded(b), seeded(c)], 3, 0, true),
        snapshot(&[&[a, b, c]], &[]),
    );
    planning.active_membership_intent = true;
    assert!(matches!(
        plan(&planning),
        PlacementRecommendation::Converging
    ));
}

/// **Contradictory policy is `InvalidPolicy`, not a best-effort plan** — for
/// each §7.7.1 contradiction, including the seed-ticket canonicity rule.
#[test]
fn contradictory_policies_are_refused_by_name() {
    let (a, b, c, d) = (node(1), node(2), node(3), node(4));
    let base = || {
        policy(
            vec![seeded(a), seeded(b), seeded(c), seeded(d)],
            3,
            0,
            false,
        )
    };
    let topo = || snapshot(&[&[a, b, c, d]], &[]);
    let expect_invalid = |planning: PlacementInput, needle: &str| match plan(&planning) {
        PlacementRecommendation::InvalidPolicy { why } => {
            assert!(why.contains(needle), "expected '{needle}' in: {why}");
        }
        other => panic!("expected InvalidPolicy({needle}), got {other:?}"),
    };

    // A pinned voter cannot be grace-replaced.
    let mut pinned_crossed = input(&[a, b, c], base(), topo());
    pinned_crossed.policy.pinned_voters.insert(a);
    pinned_crossed.grace_crossed_members.insert(a);
    expect_invalid(pinned_crossed, "grace-replaced");

    // Pins wider than the target.
    let mut wide = input(&[a, b, c], base(), topo());
    wide.policy.pinned_voters = [a, b, c, d].into_iter().collect();
    expect_invalid(wide, "exceed the target");

    // A production profile permits exactly 3 or 5.
    let mut four = input(&[a, b, c], base(), topo());
    four.policy.target_voters = 4;
    expect_invalid(four, "deployment profile");

    // The minimum cannot exceed target-choose-2.
    let mut minimum = input(&[a, b, c], base(), topo());
    minimum.policy.minimum_separated_voter_pairs = 4;
    expect_invalid(minimum, "choose 2");

    // A seed whose ticket is not the canonical Aster ticket.
    let mut forged = input(&[a, b, c], base(), topo());
    forged.policy.eligible.get_mut(&d).expect("d").member_seed = Some(MemberSeedV1 {
        node_id: d.as_bytes().to_vec(),
        ticket: "not-a-ticket".into(),
        signed_join_permit: Some(vec![1]),
    });
    expect_invalid(forged, "canonical");
}

/// **`NeedsProvisioning` names the exact NodeIds** whose seed material is
/// missing — and the same selection with seeds present becomes the plan.
#[test]
fn provisioning_names_the_exact_candidates() {
    let (v0, v1, v2, fresh) = (node(1), node(2), node(3), node(6));
    let topo = || snapshot(&[&[v0, v1, fresh]], &[]);
    let mut unprovisioned = input(
        &[v0, v1, v2],
        policy(
            vec![seeded(v0), seeded(v1), seeded(v2), unseeded(fresh)],
            3,
            0,
            false,
        ),
        topo(),
    );
    unprovisioned.grace_crossed_members.insert(v2);
    match plan(&unprovisioned) {
        PlacementRecommendation::NeedsProvisioning { candidates, .. } => {
            let expected: BTreeSet<CoordinationNodeId> = [fresh].into_iter().collect();
            assert_eq!(candidates, expected);
        }
        other => panic!("expected NeedsProvisioning, got {other:?}"),
    }

    let mut provisioned = input(
        &[v0, v1, v2],
        policy(
            vec![seeded(v0), seeded(v1), seeded(v2), seeded(fresh)],
            3,
            0,
            false,
        ),
        topo(),
    );
    provisioned.grace_crossed_members.insert(v2);
    match plan(&provisioned) {
        PlacementRecommendation::Plan(plan) => {
            assert!(plan.desired_voters.contains(&fresh));
            assert_eq!(plan.member_seeds.len(), 1);
            assert_eq!(plan.member_seeds[0].node_id, fresh.as_bytes().to_vec());
        }
        other => panic!("expected a plan, got {other:?}"),
    }
}

/// **The §15 corridor shape.** Ten producers in three positively separated
/// clusters: an explicit (administrator) rebalance from an all-in-one-cluster
/// membership spreads across clusters within the protocol's two-new-voter
/// limit; the follow-up replacement of one grace-crossed voter keeps the
/// other two voters fixed and returns exactly three.
#[test]
fn the_ten_producer_corridor_selects_across_clusters() {
    let nodes: Vec<CoordinationNodeId> = (1..=10).map(node).collect();
    let (cluster_a, cluster_b, cluster_c) = (&nodes[0..4], &nodes[4..7], &nodes[7..10]);
    let mut far = Vec::new();
    for x in cluster_a {
        for y in cluster_b.iter().chain(cluster_c) {
            far.push((*x, *y));
        }
    }
    for x in cluster_b {
        for y in cluster_c {
            far.push((*x, *y));
        }
    }
    let topo = || snapshot(&[cluster_a, cluster_b, cluster_c], &far);
    let all_seeded = || nodes.iter().map(|n| seeded(*n)).collect::<Vec<_>>();

    // The explicit spread: currently all three voters sit in cluster A, and
    // the policy demands full pairwise separation — with only the minimum
    // met, §7.7.2 step 2 would rightly keep two of them (anti-churn beats
    // spread), so the full spread must be *asked for*.
    let current = [nodes[0], nodes[1], nodes[2]];
    match plan(&input(&current, policy(all_seeded(), 3, 3, true), topo())) {
        PlacementRecommendation::Plan(plan) => {
            let clusters_hit: BTreeSet<usize> = plan
                .desired_voters
                .iter()
                .map(|v| {
                    [cluster_a, cluster_b, cluster_c]
                        .iter()
                        .position(|c| c.contains(v))
                        .expect("a member")
                })
                .collect();
            // Two new voters maximum, so at least one A-voter stays; the two
            // moves land in the two other clusters.
            assert_eq!(clusters_hit.len(), 3, "one voter per cluster: {plan:?}");
            assert!(plan.separation.diversity_proved);
        }
        other => panic!("the corridor spread found no plan: {other:?}"),
    }

    // The repair: one spread voter crosses grace; its replacement is a
    // single new voter and the two healthy voters do not move.
    let spread = [nodes[0], nodes[4], nodes[7]];
    let mut repair = input(&spread, policy(all_seeded(), 3, 1, false), topo());
    repair.grace_crossed_members.insert(nodes[4]);
    match plan(&repair) {
        PlacementRecommendation::Plan(plan) => {
            let selected: BTreeSet<CoordinationNodeId> =
                plan.desired_voters.iter().copied().collect();
            assert!(selected.contains(&nodes[0]) && selected.contains(&nodes[7]));
            assert!(
                !selected.contains(&nodes[4]),
                "the crossed voter is replaced"
            );
            assert_eq!(selected.len(), 3, "exactly three, never four");
            assert!(plan.separation.separated_pairs >= 1);
        }
        other => panic!("the corridor repair found no plan: {other:?}"),
    }
}

/// **The tie-break is the specified BLAKE3 order.** Evidence-identical
/// candidates are split by the ascending byte order of
/// `BLAKE3(group_id || "aster.coordination.placement.v1\0" || node_id)` —
/// re-derived here independently, over eight disjoint candidate pairs so a
/// drifted domain string cannot pass by coincidence.
#[test]
fn the_tie_break_is_the_specified_hash_order() {
    let (v0, v1, v2) = (node(1), node(2), node(3));
    let tag = |n: &CoordinationNodeId| {
        let mut hasher = blake3::Hasher::new();
        hasher.update(group().as_bytes());
        hasher.update(b"aster.coordination.placement.v1\0");
        hasher.update(n.as_bytes());
        *hasher.finalize().as_bytes()
    };
    for pair in 0u8..8 {
        let c1 = node(0x10 + pair * 2);
        let c2 = node(0x11 + pair * 2);
        let topo = snapshot(&[&[v0, v1, c1, c2]], &[]);
        let mut planning = input(
            &[v0, v1, v2],
            policy(
                vec![seeded(v0), seeded(v1), seeded(c1), seeded(c2)],
                3,
                0,
                false,
            ),
            topo,
        );
        // v2 is not in the eligible map at all — an administrator removal —
        // so one of the two identical candidates fills its seat.
        planning.grace_crossed_members.insert(v2);
        let expected = if tag(&c1) < tag(&c2) { c1 } else { c2 };
        match plan(&planning) {
            PlacementRecommendation::Plan(plan) => {
                assert!(
                    plan.desired_voters.contains(&expected),
                    "pair {pair}: the hash order picks {expected:?}: {plan:?}"
                );
            }
            other => panic!("pair {pair}: no plan: {other:?}"),
        }
    }
}

/// **Cardinality shortfall is `InsufficientEvidence` with the fact named.**
#[test]
fn a_cardinality_shortfall_names_the_missing_fact() {
    let (a, b) = (node(1), node(2));
    match plan(&input(
        &[a, b],
        policy(vec![seeded(a), seeded(b)], 3, 0, false),
        snapshot(&[&[a, b]], &[]),
    )) {
        PlacementRecommendation::InsufficientEvidence { missing } => {
            assert!(missing.contains("target of 3"), "{missing}");
        }
        other => panic!("expected InsufficientEvidence, got {other:?}"),
    }
}

/// **An unfillable retained-learner target is never `Stable`** (closure
/// item 4). With roles equal to current roles and no live eligible
/// candidate outside the voter set, the old planner reported health for an
/// unmet policy objective; the target is policy, and the shortfall is
/// `InsufficientEvidence`. A live eligible candidate that merely lacks seed
/// material fills the slot and flows to `NeedsProvisioning` instead.
#[test]
fn an_unfillable_learner_target_is_insufficient_evidence() {
    let (a, b, c, d) = (node(1), node(2), node(3), node(4));
    let learner_policy = |eligible: Vec<PlacementCandidate>| {
        let mut p = policy(eligible, 3, 0, false);
        p.target_retained_learners = 1;
        p
    };

    // Every eligible node is a voter: no learner candidate exists at all.
    match plan(&input(
        &[a, b, c],
        learner_policy(vec![seeded(a), seeded(b), seeded(c)]),
        snapshot(&[&[a, b, c]], &[]),
    )) {
        PlacementRecommendation::InsufficientEvidence { missing } => {
            assert!(missing.contains("retained-learner"), "{missing}");
        }
        other => panic!("expected InsufficientEvidence, got {other:?}"),
    }

    // A live eligible fourth node without a seed fills the slot and asks
    // for provisioning — the shortfall answer is reserved for a slot no
    // candidate can fill.
    match plan(&input(
        &[a, b, c],
        learner_policy(vec![seeded(a), seeded(b), seeded(c), unseeded(d)]),
        snapshot(&[&[a, b, c, d]], &[]),
    )) {
        PlacementRecommendation::NeedsProvisioning { candidates, .. } => {
            assert_eq!(candidates.into_iter().collect::<Vec<_>>(), vec![d]);
        }
        other => panic!("expected NeedsProvisioning, got {other:?}"),
    }
}

/// **A permit that does not fully verify is refused before any plan
/// exists** (closure items from reviews two and three): §7.7.1 requires
/// every supplied seed to pass **the same permit validation apply runs** —
/// signature under the pinned authority key and all four bindings — before
/// `Plan` can be emitted. Garbage bytes, a permit for another node, a
/// canonical permit for another **group**, one for another **genesis**, and
/// one whose signature was tampered are each `InvalidPolicy`; the honest
/// permit plans.
#[test]
fn a_malformed_permit_is_refused_before_any_plan() {
    let (a, b, d) = (node(1), node(2), node(4));
    let topo = || snapshot(&[&[a, b, d]], &[]);
    let refuses = |candidate: PlacementCandidate, why_name: &str| match plan(&input(
        &[a, b],
        policy(vec![seeded(a), seeded(b), candidate], 3, 0, false),
        topo(),
    )) {
        PlacementRecommendation::InvalidPolicy { why } => {
            assert!(why.contains("permit"), "{why_name}: {why}");
        }
        other => panic!("{why_name}: expected InvalidPolicy, got {other:?}"),
    };
    let with_permit = |permit: Vec<u8>| {
        let mut candidate = seeded(d);
        candidate
            .member_seed
            .as_mut()
            .expect("seeded")
            .signed_join_permit = Some(permit);
        candidate
    };

    // Garbage bytes: InvalidPolicy, not a Plan whose refusal is discovered
    // as a replicated apply failure.
    refuses(with_permit(vec![1, 2, 3]), "garbage");

    // A permit minted for a *different* node fails the candidate binding.
    refuses(
        with_permit(signed_permit(a, *group().as_bytes(), PURE_GENESIS_HASH)),
        "wrong node",
    );

    // The third review's counterexample: a canonical, correctly signed
    // permit for the same candidate but **another group** — structurally
    // perfect, and only the group binding refuses it.
    refuses(
        with_permit(signed_permit(d, [0xB7; 32], PURE_GENESIS_HASH)),
        "wrong group",
    );

    // Another genesis under the same group id.
    refuses(
        with_permit(signed_permit(d, *group().as_bytes(), [0xB8; 32])),
        "wrong genesis",
    );

    // A forged signature: the envelope names the pinned authority key, but
    // the signature was made by some other key. Only signature verification
    // under the pinned key can refuse this one.
    let forged = {
        use aster::coordination::contract::payload_fory;
        use aster::coordination::model::authority::{
            join_signing_bytes, signed_document_from_parts,
        };
        use aster::coordination::raft::JoinPermitV1;
        use ed25519_dalek::Signer;
        let honest = signed_permit(d, *group().as_bytes(), PURE_GENESIS_HASH);
        let envelope: aster::coordination::raft::SignedCoordinationDocumentV1 =
            payload_fory::<aster::coordination::raft::SignedCoordinationDocumentV1>()
                .deserialize(&honest)
                .expect("the honest envelope decodes");
        let _typed: JoinPermitV1 = payload_fory::<JoinPermitV1>()
            .deserialize(&envelope.document_bytes)
            .expect("the honest document decodes");
        let interloper = ed25519_dalek::SigningKey::from_bytes(&[0x13; 32]);
        let signature = interloper.sign(&join_signing_bytes(&envelope.document_bytes));
        signed_document_from_parts(
            envelope.document_bytes,
            &pure_authority_key(),
            &signature.to_bytes(),
        )
        .expect("the forged envelope encodes")
    };
    refuses(with_permit(forged), "forged signature");

    // The honest permit plans, and the new member's seed rides the plan.
    match plan(&input(
        &[a, b],
        policy(vec![seeded(a), seeded(b), seeded(d)], 3, 0, false),
        topo(),
    )) {
        PlacementRecommendation::Plan(plan) => {
            assert_eq!(plan.member_seeds.len(), 1);
            assert_eq!(plan.member_seeds[0].node_id, d.as_bytes().to_vec());
        }
        other => panic!("expected a Plan, got {other:?}"),
    }
}

// ── Manager-half fixtures ───────────────────────────────────────────────────

struct ScriptedTopology(Mutex<Vec<TopologySnapshot>>);

#[async_trait::async_trait]
impl TopologySource for ScriptedTopology {
    async fn snapshot(&self) -> aster::Result<TopologySnapshot> {
        let mut scripts = self.0.lock().expect("scripts");
        Ok(if scripts.len() > 1 {
            scripts.remove(0)
        } else {
            scripts[0].clone()
        })
    }
}

struct ScriptedPolicy(Mutex<Vec<PlacementPolicy>>);

impl PlacementPolicySource for ScriptedPolicy {
    fn policy(&self) -> aster::Result<PlacementPolicy> {
        let mut scripts = self.0.lock().expect("scripts");
        Ok(if scripts.len() > 1 {
            scripts.remove(0)
        } else {
            scripts[0].clone()
        })
    }
}

/// A provisioner that must never be called: every candidate the gates use is
/// pre-seeded, so provisioning is a wrong turn.
struct NoProvisioning;

#[async_trait::async_trait]
impl PlacementProvisioner for NoProvisioning {
    async fn provision(&self, request: &ProvisionRequest) -> aster::Result<MemberSeedV1> {
        panic!("provisioning was not expected for {request:?}");
    }
}

async fn start_voter(fixture: &GroupFixture, index: usize, dir: &std::path::Path) -> AsterServer {
    let voter = AsterServer::builder()
        .config(fixture.aster_config(index))
        .builtin_coordination(fixture.config(dir, index))
        .start()
        .await
        .expect("the voter starts");
    voter
}

async fn admin_client(
    fixture: &GroupFixture,
    voters: &[AsterServer],
    journal: &std::path::Path,
) -> CoordinationClient {
    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(SecretKey::from_bytes(root_secret(fixture)))
            .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
            .build(),
    )
    .await
    .expect("a client node starts");
    for voter in voters {
        node.add_node_addr(voter.node().addr()).expect("register");
    }
    let tickets: Vec<String> = (0..voters.len())
        .map(|i| {
            let id: [u8; 32] = fixture.node_id(i).try_into().expect("32 bytes");
            aster::coordination::model::authority::canonical_candidate_ticket(&id)
                .expect("a canonical ticket")
        })
        .collect();
    CoordinationClient::builder(node, fixture.group_id())
        .seed_tickets(tickets)
        .journal_dir(journal)
        .build()
        .expect("the client builds")
}

fn root_secret(fixture: &GroupFixture) -> [u8; 32] {
    let _ = fixture;
    [SEED ^ 0xFF; 32]
}

const SEED: u8 = 0xE7;

fn voter_id(fixture: &GroupFixture, index: usize) -> CoordinationNodeId {
    CoordinationNodeId::from_slice(&fixture.node_id(index)).expect("32 bytes")
}

/// A real, signed candidate seed for a fresh node under this fixture's
/// authority — the plan that carries it is accepted by apply.
fn real_candidate(fixture: &GroupFixture) -> (CoordinationNodeId, PlacementCandidate) {
    real_candidate_from(fixture, [SEED ^ 0x77; 32])
}

/// [`real_candidate`] for an explicit candidate secret, so a gate can mint
/// several distinct fixture-signed candidates.
fn real_candidate_from(
    fixture: &GroupFixture,
    secret: [u8; 32],
) -> (CoordinationNodeId, PlacementCandidate) {
    let id_bytes = ed25519_dalek::SigningKey::from_bytes(&secret)
        .verifying_key()
        .to_bytes();
    let id = CoordinationNodeId::from_slice(&id_bytes).expect("32 bytes");
    let ticket = aster::coordination::model::authority::canonical_candidate_ticket(&id_bytes)
        .expect("a canonical ticket");
    let permit = fixture.join_permit_with_ticket(secret, &ticket, &[0, 1, 2]);
    (
        id,
        PlacementCandidate {
            node_id: id,
            member_seed: Some(MemberSeedV1 {
                node_id: id_bytes.to_vec(),
                ticket,
                signed_join_permit: Some(permit),
            }),
        },
    )
}

/// The manager gate's group: three voters on their own directories, cross
/// registered, all ready.
async fn manager_group() -> (GroupFixture, Vec<AsterServer>, Vec<tempfile::TempDir>) {
    let fixture = GroupFixture::new("placement.test", SEED, 3);
    let dirs: Vec<tempfile::TempDir> = (0..3)
        .map(|_| tempfile::tempdir().expect("a directory"))
        .collect();
    let mut voters = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        voters.push(start_voter(&fixture, index, dir.path()).await);
    }
    let addrs: Vec<_> = voters.iter().map(|v| v.node().addr()).collect();
    for voter in &voters {
        for addr in &addrs {
            if addr.node_id != voter.node().addr().node_id {
                voter.node().add_node_addr(addr.clone()).expect("register");
            }
        }
    }
    for voter in &voters {
        within(
            "voter readiness",
            voter.coordination().expect("hosted").wait_ready(),
        )
        .await
        .expect("ready");
    }
    (fixture, voters, dirs)
}

/// The policy the manager gates plan under: all three voters plus the real
/// candidate eligible, one voter explicitly excluded so a replacement plan
/// exists without any grace timer.
fn replacement_policy(
    fixture: &GroupFixture,
    candidate: &PlacementCandidate,
    excluded: Option<CoordinationNodeId>,
    grace: Duration,
) -> PlacementPolicy {
    let mut eligible: BTreeMap<CoordinationNodeId, PlacementCandidate> = (0..3)
        .map(|i| {
            let id = voter_id(fixture, i);
            (id, unseeded(id))
        })
        .collect();
    eligible.insert(candidate.node_id, candidate.clone());
    PlacementPolicy {
        revision: 1,
        digest: [0xD2; 32],
        // The fixture's real authority: the same key and domain the voters
        // pin, so the candidate's fixture-signed permit fully verifies.
        authority_public_key: fixture.authority_key(),
        authority_domain: fixture.domain().to_string(),
        target_voters: 3,
        target_retained_learners: 0,
        eligible,
        pinned_voters: BTreeSet::new(),
        excluded: excluded.into_iter().collect(),
        minimum_separated_voter_pairs: 0,
        replacement_grace: grace,
        allow_healthy_rebalance: false,
    }
}

/// One cluster containing the named nodes — the live-vertex set.
fn live(nodes: &[CoordinationNodeId]) -> TopologySnapshot {
    let refs: Vec<CoordinationNodeId> = nodes.to_vec();
    snapshot(&[&refs], &[])
}

// ── The manager half ────────────────────────────────────────────────────────

/// **Two managers racing one revision commit exactly one intent.** Both
/// observe the same membership, both plan the same replacement, and the
/// revision fence plus active-intent suppression admit one; the loser
/// reports instead of double-submitting.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_racing_managers_commit_exactly_one_intent() {
    let (fixture, voters, _dirs) = manager_group().await;
    let (candidate_id, candidate) = real_candidate(&fixture);
    let excluded = voter_id(&fixture, 2);
    let members: Vec<CoordinationNodeId> = vec![
        voter_id(&fixture, 0),
        voter_id(&fixture, 1),
        voter_id(&fixture, 2),
        candidate_id,
    ];

    let journal_a = tempfile::tempdir().expect("a directory");
    let journal_b = tempfile::tempdir().expect("a directory");
    let client_a = admin_client(&fixture, &voters, journal_a.path()).await;
    let client_b = admin_client(&fixture, &voters, journal_b.path()).await;

    let manager_of = |client: CoordinationClient| {
        CoordinationPlacementManager::new(
            client,
            ScriptedTopology(Mutex::new(vec![live(&members)])),
            ScriptedPolicy(Mutex::new(vec![replacement_policy(
                &fixture,
                &candidate,
                Some(excluded),
                Duration::from_secs(600),
            )])),
            NoProvisioning,
        )
    };
    let mut manager_a = manager_of(client_a);
    let mut manager_b = manager_of(client_b);

    let (report_a, report_b) = within("the race", async {
        tokio::join!(manager_a.reconcile(), manager_b.reconcile())
    })
    .await;
    let report_a = report_a.expect("manager A reports");
    let report_b = report_b.expect("manager B reports");

    let accepted: Vec<&_> = [&report_a, &report_b]
        .into_iter()
        .filter(|r| r.pending_operation_id.is_some())
        .collect();
    assert_eq!(
        accepted.len(),
        1,
        "exactly one accepted intent:\n  A: {}\n  B: {}",
        report_a.reason,
        report_b.reason
    );
    assert!(
        accepted[0].selected_voters.contains(&candidate_id),
        "the accepted plan names the candidate"
    );

    // The group itself confirms: one pending operation, and it is the one
    // the winning report carries.
    let observer_journal = tempfile::tempdir().expect("a directory");
    let observer = admin_client(&fixture, &voters, observer_journal.path()).await;
    let status = within("status", observer.reads().group_status())
        .await
        .expect("the status cut");
    assert_eq!(
        status.pending_operation_id.as_deref(),
        accepted[0]
            .pending_operation_id
            .as_ref()
            .map(|id| id.as_slice()),
        "the committed intent is the winner's"
    );

    for voter in voters {
        voter.shutdown().await;
    }
}

/// **A restart clears an uncommitted absence timer** — replacement is
/// delayed, never accelerated — while a manager that held its timer across
/// the grace does replace. And **the recheck discards a superseded plan**:
/// when the policy revision changes between planning and submission, nothing
/// is submitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_delays_the_grace_and_the_recheck_discards_superseded_plans() {
    let (fixture, voters, _dirs) = manager_group().await;
    let (candidate_id, candidate) = real_candidate(&fixture);
    let grace = Duration::from_millis(400);
    // Voter 2 is absent from every topology snapshot; the others plus the
    // candidate are live.
    let members: Vec<CoordinationNodeId> =
        vec![voter_id(&fixture, 0), voter_id(&fixture, 1), candidate_id];
    // Policy keeps voter 2 eligible: only the grace can replace it.
    let policy_now = || replacement_policy(&fixture, &candidate, None, grace);

    let journal = tempfile::tempdir().expect("a directory");
    let client = admin_client(&fixture, &voters, journal.path()).await;
    let mut manager = CoordinationPlacementManager::new(
        client,
        ScriptedTopology(Mutex::new(vec![live(&members)])),
        ScriptedPolicy(Mutex::new(vec![policy_now()])),
        NoProvisioning,
    );

    // First observation: the absence is noted, the grace has not passed,
    // the voter is retained. Nothing is submitted.
    let first = within("first reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        first.pending_operation_id.is_none(),
        "no command before the grace: {}",
        first.reason
    );

    tokio::time::sleep(grace + Duration::from_millis(300)).await;

    // A *restarted* manager — fresh instance, empty timers — observes the
    // same absence but must start its grace from scratch.
    let journal_fresh = tempfile::tempdir().expect("a directory");
    let client_fresh = admin_client(&fixture, &voters, journal_fresh.path()).await;
    let mut restarted = CoordinationPlacementManager::new(
        client_fresh,
        ScriptedTopology(Mutex::new(vec![live(&members)])),
        ScriptedPolicy(Mutex::new(vec![policy_now()])),
        NoProvisioning,
    );
    let fresh = within("restarted reconcile", restarted.reconcile())
        .await
        .expect("a report");
    assert!(
        fresh.pending_operation_id.is_none(),
        "a restart cleared the timer; the grace restarts: {}",
        fresh.reason
    );

    // The recheck fence: the original manager's timer has crossed, but the
    // policy revision/digest advances between planning and submission — the
    // **same content**, so the freshly recomputed plan would be identical.
    // That is exactly the reviewer's trap: a recheck that merely replans and
    // submits "any fresh Plan it sees" would sail through, injecting a plan
    // (and any seeds) minted under the old `(digest, membership LogId)`
    // tuple. The retained fingerprint discards it instead.
    let mut redigested = policy_now();
    redigested.revision = 2;
    redigested.digest = [0xD3; 32];
    manager.replace_sources_for_testing(
        ScriptedTopology(Mutex::new(vec![live(&members)])),
        ScriptedPolicy(Mutex::new(vec![policy_now(), redigested])),
    );
    let superseded = within("superseded reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        superseded.pending_operation_id.is_none(),
        "the recheck discarded the superseded plan: {}",
        superseded.reason
    );
    assert!(
        superseded.reason.contains("discarded"),
        "the report names the fingerprint discard: {}",
        superseded.reason
    );

    // With the policy stable again, the crossed timer finally replaces.
    manager.replace_sources_for_testing(
        ScriptedTopology(Mutex::new(vec![live(&members)])),
        ScriptedPolicy(Mutex::new(vec![policy_now()])),
    );
    let replaced = within("replacing reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        replaced.pending_operation_id.is_some(),
        "the held timer crossed the grace and the plan submitted: {}",
        replaced.reason
    );

    for voter in voters {
        voter.shutdown().await;
    }
}

/// **A journalled membership request survives the manager's death.** The
/// replacement plan is submitted through the raw durable client, the server
/// resolves it, the process dies before acknowledging — and the next
/// manager life delivers exactly that saved resolution before any new plan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journalled_membership_request_resolves_before_new_plans() {
    let (fixture, voters, _dirs) = manager_group().await;
    let (candidate_id, candidate) = real_candidate(&fixture);
    let excluded = voter_id(&fixture, 2);
    let members = vec![
        voter_id(&fixture, 0),
        voter_id(&fixture, 1),
        voter_id(&fixture, 2),
        candidate_id,
    ];

    let journal = tempfile::tempdir().expect("a directory");
    let outcome;
    {
        // The exact plan the planner would emit, submitted raw so the death
        // can land between resolution and acknowledgement.
        let client = admin_client(&fixture, &voters, journal.path()).await;
        let status = within("status", client.reads().group_status())
            .await
            .expect("status");
        let mut desired: Vec<aster::coordination::contract::NodeIdV1> =
            [voter_id(&fixture, 0), voter_id(&fixture, 1), candidate_id]
                .iter()
                .map(|n| aster::coordination::contract::NodeIdV1 {
                    node_id: n.as_bytes().to_vec(),
                })
                .collect();
        desired.sort_by(|l, r| l.node_id.cmp(&r.node_id));
        let seed = candidate.member_seed.clone().expect("seeded");
        let resolved = within(
            "raw submission",
            client.change_membership(
                [0xAB; 16],
                status.membership_log_id.clone(),
                desired,
                Vec::new(),
                vec![seed],
            ),
        )
        .await
        .expect("resolves");
        outcome = resolved.reply().outcome_code;
        assert_eq!(outcome, 8, "the plan is accepted");
        drop(resolved); // the process dies before acknowledging
    }

    // The next life: the manager's first act is the journalled retry, not a
    // new plan.
    let client = admin_client(&fixture, &voters, journal.path()).await;
    let mut manager = CoordinationPlacementManager::new(
        client,
        ScriptedTopology(Mutex::new(vec![live(&members)])),
        ScriptedPolicy(Mutex::new(vec![replacement_policy(
            &fixture,
            &candidate,
            Some(excluded),
            Duration::from_secs(600),
        )])),
        NoProvisioning,
    );
    let report = within("resuming reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        report.reason.contains("journalled membership request"),
        "the journalled request resolved first: {}",
        report.reason
    );
    assert!(
        report.reason.contains(&format!("outcome {outcome}")),
        "the redelivered outcome is the saved one: {}",
        report.reason
    );

    // And the reconciliation after that suppresses planning: the intent the
    // journalled request created is still converging.
    let after = within("post-resume reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        after.pending_operation_id.is_some() || after.reason.contains("converging"),
        "the active intent suppresses a second plan: {}",
        after.reason
    );
    assert!(
        after.reason.contains("converging"),
        "no second plan while converging: {}",
        after.reason
    );

    for voter in voters {
        voter.shutdown().await;
    }
}

/// **No quorum: a report, never a command.** With two of three voters gone,
/// the manager's status read cannot linearize; the reconciliation reports
/// that and submits nothing — topology cannot force membership.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_quorum_produces_a_report_and_no_command() {
    let (fixture, mut voters, _dirs) = manager_group().await;
    let (candidate_id, candidate) = real_candidate(&fixture);
    let members = vec![
        voter_id(&fixture, 0),
        voter_id(&fixture, 1),
        voter_id(&fixture, 2),
        candidate_id,
    ];
    let journal = tempfile::tempdir().expect("a directory");
    let client = admin_client(&fixture, &voters, journal.path()).await;

    // Quorum dies: two of three voters shut down.
    voters.remove(2).shutdown().await;
    voters.remove(1).shutdown().await;

    let mut manager = CoordinationPlacementManager::new(
        client,
        ScriptedTopology(Mutex::new(vec![live(&members)])),
        ScriptedPolicy(Mutex::new(vec![replacement_policy(
            &fixture,
            &candidate,
            Some(voter_id(&fixture, 2)),
            Duration::from_secs(600),
        )])),
        NoProvisioning,
    );
    let report = within("no-quorum reconcile", manager.reconcile())
        .await
        .expect("a report, not an error");
    assert!(
        report.pending_operation_id.is_none(),
        "nothing was submitted: {}",
        report.reason
    );
    assert!(
        report.reason.contains("no leader or no quorum")
            || report.reason.contains("nothing is submitted"),
        "the report names the stop: {}",
        report.reason
    );

    for voter in voters {
        voter.shutdown().await;
    }
}

/// **An unresolvable journalled request stops the reconciliation cold**
/// (closure item 3). A membership request fsyncs `Pending` before its send;
/// with the quorum gone the surviving voter refuses it and the sequence
/// stays open. The next reconciliation's resume cannot resolve it, and the
/// reconciliation must surface that instead of observing, provisioning or
/// planning anything — the earlier `if let Ok(Some(..))` swallowed the
/// error and carried on. `NoProvisioning` panics if touched, so the
/// no-work claim is enforced, not assumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unresolvable_journalled_request_stops_reconciliation() {
    let (fixture, mut voters, _dirs) = manager_group().await;
    let (candidate_id, candidate) = real_candidate(&fixture);
    let members = vec![
        voter_id(&fixture, 0),
        voter_id(&fixture, 1),
        voter_id(&fixture, 2),
        candidate_id,
    ];
    let journal = tempfile::tempdir().expect("a directory");
    // The client is seeded with voter 0 alone, so every routing walk speaks
    // to exactly one live, promptly-refusing member — the test's timing
    // never depends on dial timeouts to dead nodes.
    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(SecretKey::from_bytes(root_secret(&fixture)))
            .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
            .build(),
    )
    .await
    .expect("a client node starts");
    node.add_node_addr(voters[0].node().addr())
        .expect("register");
    let id: [u8; 32] = fixture.node_id(0).try_into().expect("32 bytes");
    let client = CoordinationClient::builder(node, fixture.group_id())
        .seed_tickets([
            aster::coordination::model::authority::canonical_candidate_ticket(&id)
                .expect("a canonical ticket"),
        ])
        .journal_dir(journal.path())
        .build()
        .expect("the client builds");

    // The quorum dies (voter 0 stays reachable so the refusals are prompt),
    // then a mutation is attempted: the journal fsyncs the exact request as
    // Pending before any send, and no quorum can resolve it.
    voters.remove(2).shutdown().await;
    voters.remove(1).shutdown().await;
    let mut desired: Vec<aster::coordination::contract::NodeIdV1> =
        [voter_id(&fixture, 0), voter_id(&fixture, 1), candidate_id]
            .iter()
            .map(|n| aster::coordination::contract::NodeIdV1 {
                node_id: n.as_bytes().to_vec(),
            })
            .collect();
    desired.sort_by(|l, r| l.node_id.cmp(&r.node_id));
    let seed = candidate.member_seed.clone().expect("seeded");
    let unresolved = within(
        "doomed submission",
        client.change_membership([0xAD; 16], None, desired, Vec::new(), vec![seed]),
    )
    .await;
    assert!(unresolved.is_err(), "no quorum can resolve the request");

    // The reconciliation's first act is the resume, and the resume's
    // failure is the whole answer: no observation, no provisioning, no
    // plan. A reachability/quorum failure is a stop-report naming the
    // unresolved request; an exhausted drive surfaces as the error it is.
    // Either way nothing runs after it — the panicking provisioner proves
    // that.
    let mut manager = CoordinationPlacementManager::new(
        client,
        ScriptedTopology(Mutex::new(vec![live(&members)])),
        ScriptedPolicy(Mutex::new(vec![replacement_policy(
            &fixture,
            &candidate,
            None,
            Duration::from_secs(600),
        )])),
        NoProvisioning,
    );
    match within("stopped reconcile", manager.reconcile()).await {
        Err(_) => {}
        Ok(report) => {
            assert!(
                report.pending_operation_id.is_none() && report.reason.contains("unresolved"),
                "an unresolved journal permits no new work: {}",
                report.reason
            );
        }
    }

    for voter in voters {
        voter.shutdown().await;
    }
}

/// **A topology-only change between planning and submission discards the
/// pass** (the third review's item 2). The policy and the membership are
/// byte-identical across both observations — only the topology snapshot
/// changes, and with it the selected replacement: the first selects D, the
/// recheck's selects E. §7.7.1 says any changed selected topology fact
/// discards the earlier recommendation; a manager that merely replans and
/// submits "whatever the second snapshot produced" would commit E under a
/// pass that chose D. With the same snapshot replayed, the same pass
/// submits — proving the discard is the selection comparison and nothing
/// else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_topology_only_change_discards_the_recommendation() {
    let (fixture, voters, _dirs) = manager_group().await;
    let (candidate_d, seed_d) = real_candidate_from(&fixture, [SEED ^ 0x77; 32]);
    let (candidate_e, seed_e) = real_candidate_from(&fixture, [SEED ^ 0x78; 32]);
    let grace = Duration::from_millis(400);

    // Voter 2 is absent from every snapshot; only the replacement candidate
    // differs between the two worlds.
    let world_d = vec![voter_id(&fixture, 0), voter_id(&fixture, 1), candidate_d];
    let world_e = vec![voter_id(&fixture, 0), voter_id(&fixture, 1), candidate_e];

    // One policy for the whole test: both candidates eligible and seeded,
    // voter 2 retained-eligible so only the grace can replace it.
    let policy_now = || {
        let mut policy = replacement_policy(&fixture, &seed_d, None, grace);
        policy.eligible.insert(candidate_e, seed_e.clone());
        policy
    };

    let journal = tempfile::tempdir().expect("a directory");
    let client = admin_client(&fixture, &voters, journal.path()).await;
    let mut manager = CoordinationPlacementManager::new(
        client,
        ScriptedTopology(Mutex::new(vec![live(&world_d)])),
        ScriptedPolicy(Mutex::new(vec![policy_now()])),
        NoProvisioning,
    );

    // First reconciliation starts the grace timer; nothing is submitted.
    let first = within("first reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        first.pending_operation_id.is_none(),
        "no command before the grace: {}",
        first.reason
    );
    tokio::time::sleep(grace + Duration::from_millis(300)).await;

    // The grace has crossed. The planning observation sees world D and
    // selects D; the recheck observation sees world E and would select E.
    // Same policy, same membership — the pass must be discarded.
    manager.replace_sources_for_testing(
        ScriptedTopology(Mutex::new(vec![live(&world_d), live(&world_e)])),
        ScriptedPolicy(Mutex::new(vec![policy_now()])),
    );
    let discarded = within("topology-shifted reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        discarded.pending_operation_id.is_none(),
        "a shifted selection must not be submitted: {}",
        discarded.reason
    );
    assert!(
        discarded.reason.contains("selected topology facts"),
        "the report names the topology discard: {}",
        discarded.reason
    );

    // The group agrees nothing was committed.
    let observer_journal = tempfile::tempdir().expect("a directory");
    let observer = admin_client(&fixture, &voters, observer_journal.path()).await;
    let status = within("status", observer.reads().group_status())
        .await
        .expect("the status cut");
    assert!(
        status.pending_operation_id.is_none(),
        "no intent was created by the discarded pass"
    );

    // A stable topology submits: same manager, same policy, one world.
    manager.replace_sources_for_testing(
        ScriptedTopology(Mutex::new(vec![live(&world_d)])),
        ScriptedPolicy(Mutex::new(vec![policy_now()])),
    );
    let submitted = within("stable reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        submitted.pending_operation_id.is_some(),
        "the same pass under a stable topology submits: {}",
        submitted.reason
    );
    assert!(
        submitted.selected_voters.contains(&candidate_d),
        "the submitted plan names the selection the pass was planned for"
    );

    for voter in voters {
        voter.shutdown().await;
    }
}

/// **The same selected nodes under changed topology evidence discard the
/// pass** (the fourth review's item 3). Selection identity is not the
/// fingerprint: here the planning observation and the recheck observation
/// select the *identical* voters, but the pairwise separation evidence
/// behind them moves from corroborated `Separated` to `Unknown` between
/// the two snapshots. §7.7.1 discards on any changed selected topology
/// fact — a pass planned on proved separation must not be submitted on
/// unproved separation. The same evidence replayed submits, pinning the
/// discard to the evidence comparison and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_separation_evidence_discards_the_same_selection() {
    let (fixture, voters, _dirs) = manager_group().await;
    let (candidate_id, candidate) = real_candidate(&fixture);
    let grace = Duration::from_millis(400);
    let members = [voter_id(&fixture, 0), voter_id(&fixture, 1), candidate_id];

    // Each member in its own cluster; `corroborated` controls whether every
    // cross-cluster pair carries far-pair evidence (Separated) or none
    // (Unknown). The member set — and therefore the selection — is the same
    // in both worlds.
    let world = |corroborated: bool| {
        let clusters: Vec<&[CoordinationNodeId]> =
            members.iter().map(std::slice::from_ref).collect();
        let far: Vec<(CoordinationNodeId, CoordinationNodeId)> = if corroborated {
            let mut pairs = Vec::new();
            for (i, a) in members.iter().enumerate() {
                for b in &members[i + 1..] {
                    pairs.push((*a, *b));
                }
            }
            pairs
        } else {
            Vec::new()
        };
        snapshot(&clusters, &far)
    };

    let policy_now = || replacement_policy(&fixture, &candidate, None, grace);
    let journal = tempfile::tempdir().expect("a directory");
    let client = admin_client(&fixture, &voters, journal.path()).await;
    let mut manager = CoordinationPlacementManager::new(
        client,
        ScriptedTopology(Mutex::new(vec![world(true)])),
        ScriptedPolicy(Mutex::new(vec![policy_now()])),
        NoProvisioning,
    );

    // Start the grace timer for the absent voter 2.
    let first = within("first reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(first.pending_operation_id.is_none(), "{}", first.reason);
    tokio::time::sleep(grace + Duration::from_millis(300)).await;

    // Planning observation: corroborated separation. Recheck observation:
    // the same members, the same selection — but every voter pair's verdict
    // fell to Unknown. The pass must be discarded.
    manager.replace_sources_for_testing(
        ScriptedTopology(Mutex::new(vec![world(true), world(false)])),
        ScriptedPolicy(Mutex::new(vec![policy_now()])),
    );
    let discarded = within("evidence-shifted reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        discarded.pending_operation_id.is_none(),
        "moved evidence must not be submitted: {}",
        discarded.reason
    );
    assert!(
        discarded.reason.contains("selected topology facts"),
        "the report names the discard: {}",
        discarded.reason
    );

    // Stable evidence submits the identical pass.
    manager.replace_sources_for_testing(
        ScriptedTopology(Mutex::new(vec![world(true)])),
        ScriptedPolicy(Mutex::new(vec![policy_now()])),
    );
    let submitted = within("stable reconcile", manager.reconcile())
        .await
        .expect("a report");
    assert!(
        submitted.pending_operation_id.is_some(),
        "the same pass under stable evidence submits: {}",
        submitted.reason
    );

    for voter in voters {
        voter.shutdown().await;
    }
}
