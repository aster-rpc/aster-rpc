//! §7.7's pure, deterministic voter placement planner (`DECIDED
//! S4-PLACEMENT`: Aster selects; application policy constrains; Raft
//! authorizes).
//!
//! ## The boundary
//!
//! Everything here is **local advice**. The planner reads one linearizable
//! membership observation, one immutable [`TopologySnapshot`], and one
//! application-owned [`PlacementPolicy`], and returns a recommendation. It
//! adds no replicated row, no Store method and no wire root; the only way a
//! recommendation changes membership is the normal durable
//! `change_membership` journal, authorized like any other administrator
//! mutation. Eligibility, pins, exclusions, replacement grace and
//! provisioning are the application's; producer admission or possession of
//! the topology namespace is never eligibility.
//!
//! ## Determinism
//!
//! For the same typed input the planner returns the same result independent
//! of map or set insertion order: every collection is ordered, candidate
//! subsets are enumerated in the group-specific tie-break order, and the
//! final tie-break is the ascending byte order of
//! `BLAKE3(group_id || "aster.coordination.placement.v1\0" || node_id)`.
//! The topology bridge score is structurally absent — the planner never
//! reads a bridge or witness field.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::coordination::contract::{LogIdV1, MemberSeedV1};
use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
use crate::coordination::model::shape::{
    profile_permits_voters, MAX_NEW_VOTERS, MAX_RETAINED_LEARNERS,
};
use crate::topology::{SeparationVerdict, TopologySnapshot};

/// The tie-break domain separator (§7.7.2 step 5).
const TIE_BREAK_DOMAIN: &[u8] = b"aster.coordination.placement.v1\0";

/// One exact candidate: a NodeId, optionally with the canonical seed
/// material needed to provision/join it.
#[derive(Debug, Clone)]
pub struct PlacementCandidate {
    pub node_id: CoordinationNodeId,
    pub member_seed: Option<MemberSeedV1>,
}

/// The application-owned policy (§7.7.1). `digest` is the application's
/// BLAKE3 of its canonical verified policy record; Aster compares the opaque
/// bytes during revalidation and never re-encodes the policy.
#[derive(Debug, Clone)]
pub struct PlacementPolicy {
    pub revision: u64,
    pub digest: [u8; 32],
    pub target_voters: u8,
    pub target_retained_learners: u8,
    pub eligible: BTreeMap<CoordinationNodeId, PlacementCandidate>,
    pub pinned_voters: BTreeSet<CoordinationNodeId>,
    pub excluded: BTreeSet<CoordinationNodeId>,
    /// The policy's explicit choice between separation as requirement
    /// (positive) and separation as preference (zero).
    pub minimum_separated_voter_pairs: u8,
    pub replacement_grace: Duration,
    pub allow_healthy_rebalance: bool,
}

/// One planning input: policy, one topology snapshot, and one linearizable
/// committed-membership observation.
#[derive(Clone)]
pub struct PlacementInput {
    pub group_id: CoordinationGroupId,
    pub deployment_profile_code: i32,
    pub membership_log_id: Option<LogIdV1>,
    pub current_voters: BTreeSet<CoordinationNodeId>,
    pub current_learners: BTreeSet<CoordinationNodeId>,
    pub active_membership_intent: bool,
    /// Members whose absence the **manager** has observed continuously for
    /// the full replacement grace. The planner never reconstructs timers.
    pub grace_crossed_members: BTreeSet<CoordinationNodeId>,
    pub policy: PlacementPolicy,
    pub topology: TopologySnapshot,
}

/// Pairwise separation evidence for one selected voter set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeparationSummary {
    pub separated_pairs: u8,
    pub connected_pairs: u8,
    pub unknown_pairs: u8,
    pub total_pairs: u8,
    /// Distinct known-close clusters represented by selected voters that are
    /// live vertices. Never a rack, zone or geography claim.
    pub represented_clusters: u8,
    /// True only when every pair carries positive `Separated` evidence.
    /// `Unknown` reaches cardinality but leaves diversity unproved.
    pub diversity_proved: bool,
}

/// A complete desired membership, ready to become a `MembershipPlanV1`.
#[derive(Debug, Clone)]
pub struct PlacementPlan {
    /// Ascending byte order, as the wire plan requires.
    pub desired_voters: Vec<CoordinationNodeId>,
    pub retained_learners: Vec<CoordinationNodeId>,
    /// Seeds for exactly the plan's new members.
    pub member_seeds: Vec<MemberSeedV1>,
    /// The membership LogId this plan was selected against — the revision
    /// fence the submission must carry.
    pub expected_membership_log_id: Option<LogIdV1>,
    pub separation: SeparationSummary,
}

/// The planner's answer (§7.7.2).
#[derive(Debug, Clone)]
pub enum PlacementRecommendation {
    /// Current desired roles already satisfy policy.
    Stable {
        separation: SeparationSummary,
    },
    /// Cardinality and authority are intact, but a topology objective is
    /// unmet and policy does not authorize moving a healthy voter.
    Degraded {
        why: String,
        separation: SeparationSummary,
    },
    /// One membership intent is active; no second plan exists.
    Converging,
    /// Exactly these selected NodeIds lack canonical seed material.
    NeedsProvisioning {
        candidates: BTreeSet<CoordinationNodeId>,
    },
    /// Required eligible/live candidates or requested positive separation
    /// are unavailable; `missing` names the fact.
    InsufficientEvidence {
        missing: String,
    },
    /// Contradictory pins/exclusions/count/profile.
    InvalidPolicy {
        why: String,
    },
    Plan(Box<PlacementPlan>),
}

/// The advisory local status one reconciliation produces. Never a
/// `group_status` field: a voter has no authoritative copy of external
/// policy, so this lives beside the manager that does.
#[derive(Debug, Clone)]
pub struct PlacementReport {
    pub policy_revision: u64,
    pub policy_digest: [u8; 32],
    pub membership_log_id: Option<LogIdV1>,
    pub current_voters: Vec<CoordinationNodeId>,
    pub current_learners: Vec<CoordinationNodeId>,
    pub selected_voters: Vec<CoordinationNodeId>,
    pub selected_learners: Vec<CoordinationNodeId>,
    pub separation: Option<SeparationSummary>,
    pub pending_operation_id: Option<[u8; 16]>,
    pub reason: String,
}

/// §7.7.2 step 5: the group-specific tie-break tag.
pub(crate) fn tie_break_tag(group: &CoordinationGroupId, node: &CoordinationNodeId) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(group.as_bytes());
    hasher.update(TIE_BREAK_DOMAIN);
    hasher.update(node.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Pairwise separation evidence over a candidate voter set.
fn summarize(input: &PlacementInput, voters: &BTreeSet<CoordinationNodeId>) -> SeparationSummary {
    let hexes: Vec<String> = voters.iter().map(|n| n.to_hex()).collect();
    let mut separated = 0u8;
    let mut connected = 0u8;
    let mut unknown = 0u8;
    for (i, a) in hexes.iter().enumerate() {
        for b in &hexes[i + 1..] {
            match input.topology.separated_hex(a, b) {
                SeparationVerdict::Separated => separated += 1,
                SeparationVerdict::Connected => connected += 1,
                SeparationVerdict::Unknown => unknown += 1,
            }
        }
    }
    let clusters: BTreeSet<usize> = hexes
        .iter()
        .filter_map(|hex| input.topology.cluster_index_of_hex(hex))
        .collect();
    let total = separated + connected + unknown;
    SeparationSummary {
        separated_pairs: separated,
        connected_pairs: connected,
        unknown_pairs: unknown,
        total_pairs: total,
        represented_clusters: u8::try_from(clusters.len()).unwrap_or(u8::MAX),
        diversity_proved: total > 0 && separated == total,
    }
}

/// The §7.7.1 policy validation. `Err` is the `InvalidPolicy` text.
fn validate_policy(input: &PlacementInput) -> Result<(), String> {
    let policy = &input.policy;
    let target = usize::from(policy.target_voters);
    // Despite the name, `profile_permits_voters` answers "does this count
    // violate the profile" — true means refuse.
    if profile_permits_voters(input.deployment_profile_code, target) {
        return Err(format!(
            "target_voters {} is not permitted by deployment profile {}",
            policy.target_voters, input.deployment_profile_code
        ));
    }
    if usize::from(policy.target_retained_learners) > MAX_RETAINED_LEARNERS {
        return Err(format!(
            "target_retained_learners {} exceeds the shape limit {MAX_RETAINED_LEARNERS}",
            policy.target_retained_learners
        ));
    }
    let max_pairs = policy.target_voters as u32 * (policy.target_voters as u32 - 1) / 2;
    if u32::from(policy.minimum_separated_voter_pairs) > max_pairs {
        return Err(format!(
            "minimum_separated_voter_pairs {} exceeds target_voters choose 2 = {max_pairs}",
            policy.minimum_separated_voter_pairs
        ));
    }
    if policy.pinned_voters.len() > target {
        return Err(format!(
            "{} pinned voters exceed the target of {}",
            policy.pinned_voters.len(),
            policy.target_voters
        ));
    }
    for pin in &policy.pinned_voters {
        if !policy.eligible.contains_key(pin) {
            return Err(format!("pinned voter {} is not eligible", pin.to_hex()));
        }
        if policy.excluded.contains(pin) {
            return Err(format!("pinned voter {} is also excluded", pin.to_hex()));
        }
        if input.grace_crossed_members.contains(pin) {
            return Err(format!(
                "pinned voter {} cannot be grace-replaced",
                pin.to_hex()
            ));
        }
    }
    for (key, candidate) in &policy.eligible {
        if candidate.node_id != *key {
            return Err(format!(
                "eligible entry {} names a different node {}",
                key.to_hex(),
                candidate.node_id.to_hex()
            ));
        }
        if let Some(seed) = &candidate.member_seed {
            if seed.node_id.as_slice() != key.as_bytes().as_slice() {
                return Err(format!(
                    "the seed for {} names a different node",
                    key.to_hex()
                ));
            }
            // The same canonical-ticket validation a directly submitted plan
            // passes at apply, run before a plan can be emitted at all.
            if crate::coordination::model::shape::check_seed_ticket(
                seed.ticket.as_bytes(),
                key.as_bytes(),
            )
            .is_err()
            {
                return Err(format!(
                    "the seed ticket for {} is not the canonical Aster ticket",
                    key.to_hex()
                ));
            }
        }
    }
    Ok(())
}

/// One enumerated voter set, scored.
struct Scored {
    voters: BTreeSet<CoordinationNodeId>,
    meets_minimum: bool,
    /// Grace-crossed members kept in the set — replacement is what the
    /// application authorized, so fewer is better, ranked above retention
    /// so anti-churn cannot cancel an authorized repair.
    grace_retained: usize,
    retained: usize,
    summary: SeparationSummary,
    tie: Vec<[u8; 32]>,
}

impl Scored {
    /// The §7.7.2 lexicographic objective, "better" first.
    fn beats(&self, other: &Scored) -> bool {
        (
            self.meets_minimum,
            std::cmp::Reverse(self.grace_retained),
            self.retained,
            self.summary.separated_pairs,
            self.summary.represented_clusters,
            std::cmp::Reverse(self.tie.clone()),
        ) > (
            other.meets_minimum,
            std::cmp::Reverse(other.grace_retained),
            other.retained,
            other.summary.separated_pairs,
            other.summary.represented_clusters,
            std::cmp::Reverse(other.tie.clone()),
        )
    }
}

/// Enumerate every voter set of the target size from `universe` that
/// honours `required` membership and the new-voter cap, and return the best
/// under the lexicographic objective. `None` when no set is feasible.
fn best_voter_set(
    input: &PlacementInput,
    universe: &[CoordinationNodeId],
    required: &BTreeSet<CoordinationNodeId>,
    max_new: usize,
) -> Option<Scored> {
    let target = usize::from(input.policy.target_voters);
    let minimum = input.policy.minimum_separated_voter_pairs;
    let mut best: Option<Scored> = None;
    let mut chosen: Vec<usize> = Vec::with_capacity(target);

    #[allow(clippy::too_many_arguments)]
    fn recurse(
        input: &PlacementInput,
        universe: &[CoordinationNodeId],
        required: &BTreeSet<CoordinationNodeId>,
        max_new: usize,
        target: usize,
        minimum: u8,
        start: usize,
        chosen: &mut Vec<usize>,
        best: &mut Option<Scored>,
    ) {
        if chosen.len() == target {
            let voters: BTreeSet<CoordinationNodeId> =
                chosen.iter().map(|i| universe[*i]).collect();
            if !required.is_subset(&voters) {
                return;
            }
            let new = voters.difference(&input.current_voters).count();
            if new > max_new {
                return;
            }
            let summary = summarize(input, &voters);
            let mut tie: Vec<[u8; 32]> = voters
                .iter()
                .map(|n| tie_break_tag(&input.group_id, n))
                .collect();
            tie.sort_unstable();
            let scored = Scored {
                meets_minimum: summary.separated_pairs >= minimum,
                grace_retained: voters.intersection(&input.grace_crossed_members).count(),
                retained: voters.intersection(&input.current_voters).count(),
                summary,
                tie,
                voters,
            };
            if best.as_ref().is_none_or(|held| scored.beats(held)) {
                *best = Some(scored);
            }
            return;
        }
        let remaining = target - chosen.len();
        for index in start..universe.len() {
            if universe.len() - index < remaining {
                break;
            }
            chosen.push(index);
            recurse(
                input,
                universe,
                required,
                max_new,
                target,
                minimum,
                index + 1,
                chosen,
                best,
            );
            chosen.pop();
        }
    }

    recurse(
        input,
        universe,
        required,
        max_new,
        target,
        minimum,
        0,
        &mut chosen,
        &mut best,
    );
    best
}

/// §7.7.2: the pure selection. Same typed input, same answer, independent of
/// insertion order.
pub fn plan(input: &PlacementInput) -> PlacementRecommendation {
    if let Err(why) = validate_policy(input) {
        return PlacementRecommendation::InvalidPolicy { why };
    }
    if input.active_membership_intent {
        return PlacementRecommendation::Converging;
    }
    let policy = &input.policy;
    let target = usize::from(policy.target_voters);

    // A current member may be retained while temporarily absent from
    // topology; a *new* candidate must be a live vertex of this snapshot.
    let retainable = |node: &CoordinationNodeId| {
        policy.eligible.contains_key(node) && !policy.excluded.contains(node)
    };
    let live = |node: &CoordinationNodeId| input.topology.contains_hex(&node.to_hex());

    let mut universe: BTreeSet<CoordinationNodeId> = BTreeSet::new();
    for voter in &input.current_voters {
        if retainable(voter) {
            universe.insert(*voter);
        }
    }
    for node in policy.eligible.keys() {
        if !policy.excluded.contains(node) && !input.current_voters.contains(node) && live(node) {
            universe.insert(*node);
        }
    }
    for pin in &policy.pinned_voters {
        if !universe.contains(pin) {
            return PlacementRecommendation::InsufficientEvidence {
                missing: format!(
                    "pinned voter {} is neither a retainable current member nor a live eligible candidate",
                    pin.to_hex()
                ),
            };
        }
    }
    if universe.len() < target {
        return PlacementRecommendation::InsufficientEvidence {
            missing: format!(
                "only {} retainable or live eligible candidates exist for a target of {}",
                universe.len(),
                target
            ),
        };
    }

    // Deterministic enumeration order: the group-specific tie-break.
    let mut ordered: Vec<CoordinationNodeId> = universe.iter().copied().collect();
    ordered.sort_by_key(|n| tie_break_tag(&input.group_id, n));

    // A protected current voter is eligible, not excluded, not
    // grace-crossed, and inside an unchanged target count. Protection is
    // immovability only for ordinary automatic maintenance; explicit pins
    // are stronger administrator intent and take precedence when the two
    // conflict.
    let unchanged_count = target == input.current_voters.len();
    let explicit = policy.allow_healthy_rebalance || !unchanged_count;
    let protected: BTreeSet<CoordinationNodeId> = input
        .current_voters
        .iter()
        .filter(|v| retainable(v) && !input.grace_crossed_members.contains(*v))
        .copied()
        .collect();
    let mut required: BTreeSet<CoordinationNodeId> = policy.pinned_voters.clone();
    if !explicit {
        let with_protected: BTreeSet<CoordinationNodeId> =
            required.union(&protected).copied().collect();
        if with_protected.len() <= target {
            required = with_protected;
        }
    }
    // Ordinary automatic maintenance introduces at most one new voter per
    // plan; an explicit operation may use the protocol limit.
    let max_new = if explicit { MAX_NEW_VOTERS } else { 1 };

    let Some(best) = best_voter_set(input, &ordered, &required, max_new) else {
        return PlacementRecommendation::InsufficientEvidence {
            missing: format!(
                "no voter set of size {target} honours the pins and the new-voter cap"
            ),
        };
    };

    if policy.minimum_separated_voter_pairs > 0 && !best.meets_minimum {
        // Would relaxing protection (an explicit rebalance) meet it? Then
        // the honest answer is Degraded, not InsufficientEvidence: the
        // evidence exists, policy just does not authorize the move.
        let relaxed = best_voter_set(input, &ordered, &policy.pinned_voters, MAX_NEW_VOTERS);
        if relaxed.as_ref().is_some_and(|r| r.meets_minimum) {
            return PlacementRecommendation::Degraded {
                why: format!(
                    "meeting {} separated pairs requires moving a healthy voter, and \
                     allow_healthy_rebalance is false",
                    policy.minimum_separated_voter_pairs
                ),
                separation: best.summary,
            };
        }
        return PlacementRecommendation::InsufficientEvidence {
            missing: format!(
                "no eligible live candidate set attains {} separated pairs; the best \
                 attainable is {}",
                policy.minimum_separated_voter_pairs, best.summary.separated_pairs
            ),
        };
    }

    // §7.7.2 step 4: preserve eligible current retained learners that are
    // neither excluded nor grace-crossed, then fill remaining learner slots
    // outside the voter set by the same evidence objective.
    let learner_target = usize::from(policy.target_retained_learners);
    let mut learners: BTreeSet<CoordinationNodeId> = input
        .current_learners
        .iter()
        .filter(|l| {
            retainable(l) && !input.grace_crossed_members.contains(*l) && !best.voters.contains(*l)
        })
        .copied()
        .collect();
    while learners.len() > learner_target {
        // Trim by descending tie-break, so the kept set is deterministic.
        let drop = *learners
            .iter()
            .max_by_key(|n| tie_break_tag(&input.group_id, n))
            .expect("nonempty");
        learners.remove(&drop);
    }
    let mut fillers: Vec<CoordinationNodeId> = ordered
        .iter()
        .filter(|n| {
            !best.voters.contains(*n)
                && !learners.contains(*n)
                && !input.current_voters.contains(*n)
                && live(n)
        })
        .copied()
        .collect();
    fillers.sort_by_key(|n| {
        // Prefer a candidate whose cluster is not yet represented among the
        // selected voters, then the tie-break.
        let voter_clusters: BTreeSet<usize> = best
            .voters
            .iter()
            .filter_map(|v| input.topology.cluster_index_of_hex(&v.to_hex()))
            .collect();
        let already_represented = input
            .topology
            .cluster_index_of_hex(&n.to_hex())
            .is_none_or(|c| voter_clusters.contains(&c));
        (already_represented, tie_break_tag(&input.group_id, n))
    });
    for filler in fillers {
        if learners.len() >= learner_target {
            break;
        }
        learners.insert(filler);
    }

    if best.voters == input.current_voters && learners == input.current_learners {
        // Retaining a grace-crossed member because nothing can replace it is
        // not stability; say so instead of reporting health.
        if best.grace_retained > 0 {
            return PlacementRecommendation::Degraded {
                why: format!(
                    "{} grace-crossed members cannot be replaced: no live eligible                      candidate fills their seats",
                    best.grace_retained
                ),
                separation: best.summary,
            };
        }
        return PlacementRecommendation::Stable {
            separation: best.summary,
        };
    }

    // Seeds for exactly the new members; a new member without a canonical
    // seed (a new node also needs its signed join permit) is what the
    // application must provision.
    let current: BTreeSet<CoordinationNodeId> = input
        .current_voters
        .union(&input.current_learners)
        .copied()
        .collect();
    let mut member_seeds = Vec::new();
    let mut unprovisioned: BTreeSet<CoordinationNodeId> = BTreeSet::new();
    for member in best.voters.union(&learners) {
        if current.contains(member) {
            continue;
        }
        match policy
            .eligible
            .get(member)
            .and_then(|c| c.member_seed.as_ref())
        {
            Some(seed) if seed.signed_join_permit.is_some() => member_seeds.push(seed.clone()),
            _ => {
                unprovisioned.insert(*member);
            }
        }
    }
    if !unprovisioned.is_empty() {
        return PlacementRecommendation::NeedsProvisioning {
            candidates: unprovisioned,
        };
    }
    member_seeds.sort_by(|l, r| l.node_id.cmp(&r.node_id));

    let mut desired_voters: Vec<CoordinationNodeId> = best.voters.iter().copied().collect();
    desired_voters.sort_by_key(|n| *n.as_bytes());
    let mut retained_learners: Vec<CoordinationNodeId> = learners.into_iter().collect();
    retained_learners.sort_by_key(|n| *n.as_bytes());
    PlacementRecommendation::Plan(Box::new(PlacementPlan {
        desired_voters,
        retained_learners,
        member_seeds,
        expected_membership_log_id: input.membership_log_id.clone(),
        separation: best.summary,
    }))
}
