//! §7.7.3's optional placement manager.
//!
//! `CoordinationPlacementManager` runs **beside an application
//! administrator**, not inside `CoordinationRuntime` and not once per voter.
//! It owns no authority secret: eligibility, pins, exclusions and
//! provisioning come from the application through the two traits here, and
//! the only way its recommendation touches membership is the normal durable
//! [`CoordinationClient::change_membership`] journal — authorized by the
//! replicated administrator predicate like any other mutation, fenced by the
//! membership revision it planned against.
//!
//! One reconciliation is §7.7.3's sequence exactly: deliver any journalled
//! retry first, read one linearizable `group_status` cut, stop on no
//! leader/quorum, joint membership or an active intent, take one atomic
//! topology snapshot, apply the manager-local replacement grace, run the
//! pure planner, provision missing seeds through the idempotent typed tuple,
//! then **re-observe and recompute before submitting** — a changed
//! membership LogId, policy revision/digest, eligibility or topology fact
//! discards the earlier recommendation instead of racing it.
//!
//! ## Absence hysteresis
//!
//! The grace timer is manager-local safety state: a current member enters
//! the replaceable set only after being absent from **every** snapshot this
//! manager took across the full grace. A restarted manager starts with no
//! timers, delaying replacement rather than shortening the grace. An
//! explicit policy exclusion needs no timer. Crossing the grace is an
//! application-authorized replacement threshold, never proof that the
//! member or its site failed.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use super::placement::{
    self, PlacementInput, PlacementPlan, PlacementPolicy, PlacementRecommendation, PlacementReport,
};
use super::{CoordinationClient, ResolvedMutation};
use crate::coordination::contract::{LogIdV1, MemberSeedV1, NodeIdV1};
use crate::coordination::ids::CoordinationNodeId;
use crate::coordination::store::wire::GroupStatusReplyWireV1;
use crate::error::{Error, Result};
use crate::topology::TopologySnapshot;

/// The idempotency tuple one provisioning call is keyed by (§7.7.3 step 4).
/// Same tuple, same candidate — the callback must be safe to repeat.
#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    pub group_id: crate::coordination::ids::CoordinationGroupId,
    pub policy_digest: [u8; 32],
    pub membership_log_id: Option<LogIdV1>,
    pub node_id: CoordinationNodeId,
}

/// The application's provisioner: start (or find) a persistent-identity Join
/// candidate for `request.node_id` and return its canonical ticket plus
/// signed join permit. The signing authority stays outside Aster and every
/// voter.
#[async_trait::async_trait]
pub trait PlacementProvisioner: Send + Sync {
    async fn provision(&self, request: &ProvisionRequest) -> Result<MemberSeedV1>;
}

/// The application's verified policy record, re-read at every observation so
/// a revision/digest change between planning and submission discards the
/// plan.
pub trait PlacementPolicySource: Send + Sync {
    fn policy(&self) -> Result<PlacementPolicy>;
}

/// One atomic topology observation per call. Production wiring is the
/// [`Topology`](crate::topology::Topology) handle (which must itself have
/// joined the producer-only shared namespace — a view prerequisite, never
/// Store permission); harnesses supply deterministic snapshots.
#[async_trait::async_trait]
pub trait TopologySource: Send + Sync {
    async fn snapshot(&self) -> Result<TopologySnapshot>;
}

#[async_trait::async_trait]
impl TopologySource for crate::topology::Topology {
    async fn snapshot(&self) -> Result<TopologySnapshot> {
        crate::topology::Topology::snapshot(self).await
    }
}

/// What one observation pass decided.
enum Observed {
    /// Reconciliation stops here; the report says why.
    Stop(Box<PlacementReport>),
    /// The planner may run on this input.
    Ready(Box<PlacementInput>, Box<GroupStatusReplyWireV1>),
}

/// The facts a plan was computed against, retained across provisioning so
/// the §7.7.3 recheck can *prove* nothing moved rather than trusting that a
/// fresh plan is the same plan. A seed was provisioned under exactly this
/// `(policy digest, membership LogId)` tuple; submitting it after the tuple
/// changed would inject material minted for a different world.
#[derive(Debug, Clone, PartialEq)]
struct ObservationFingerprint {
    policy_revision: u64,
    policy_digest: [u8; 32],
    membership_log_id: Option<LogIdV1>,
}

impl ObservationFingerprint {
    fn of(input: &PlacementInput) -> Self {
        Self {
            policy_revision: input.policy.revision,
            policy_digest: input.policy.digest,
            membership_log_id: input.membership_log_id.clone(),
        }
    }
}

/// The whole retained recommendation: the observation tuple, the selection
/// it produced, **and the topology evidence behind that selection**.
/// §7.7.1's revalidation rule covers "any changed selected topology fact" —
/// and the same nodes can remain selected while their presence, cluster
/// equivalence or pairwise separation evidence changes underneath, so
/// selected IDs alone are not the fingerprint. The recheck recomputes the
/// evidence for the *retained* selection from the fresh snapshot and
/// discards the pass when any fact moved.
#[derive(Debug, Clone, PartialEq)]
struct PlannedPass {
    observation: ObservationFingerprint,
    desired_voters: Vec<CoordinationNodeId>,
    retained_learners: Vec<CoordinationNodeId>,
    evidence: placement::SelectionEvidence,
}

pub struct CoordinationPlacementManager<T, P, V> {
    client: CoordinationClient,
    topology: T,
    policy_source: P,
    provisioner: V,
    /// First instant each current member was observed absent, cleared on
    /// reappearance. Never persisted: restart delays, never shortens.
    absence: BTreeMap<CoordinationNodeId, Instant>,
}

impl<T, P, V> CoordinationPlacementManager<T, P, V>
where
    T: TopologySource,
    P: PlacementPolicySource,
    V: PlacementProvisioner,
{
    pub fn new(client: CoordinationClient, topology: T, policy_source: P, provisioner: V) -> Self {
        Self {
            client,
            topology,
            policy_source,
            provisioner,
            absence: BTreeMap::new(),
        }
    }

    /// Swap the observation sources — the gate's lever for "the policy
    /// changed between planning and submission".
    #[cfg(feature = "coordination-testing")]
    pub fn replace_sources_for_testing(&mut self, topology: T, policy_source: P) {
        self.topology = topology;
        self.policy_source = policy_source;
    }

    /// Run one full reconciliation and report what happened. Never edits
    /// membership except through the durable journal; never edits anything
    /// at all when the group has no leader, no quorum, a joint membership,
    /// or an active intent.
    pub async fn reconcile(&mut self) -> Result<PlacementReport> {
        // §7.7.3's "exact journalled retry": a membership request fsynced by
        // a previous life of this client resolves before anything new. An
        // error here means an older durable request **may still be
        // unresolved**, so no observation, provisioning, or planning may
        // run: reachability failures become stop-reports like the ones the
        // observation pass produces, and anything else surfaces as the
        // failure it is.
        match self.client.resume().await {
            Ok(Some(resolved)) => {
                let outcome = resolved.reply().outcome_code;
                resolved.acknowledge().await?;
                return Ok(self.stub_report(format!(
                    "a journalled membership request from a previous run resolved \
                     with outcome {outcome}; re-observe before planning again"
                )));
            }
            Ok(None) => {}
            Err(Error::Rpc {
                code: 14, message, ..
            }) => {
                return Ok(self.stub_report(format!(
                    "a journalled membership request from a previous run is still \
                     unresolved (no leader or no quorum: {message}); nothing new \
                     is planned until it resolves"
                )))
            }
            Err(Error::Transport(err)) => {
                return Ok(self.stub_report(format!(
                    "a journalled membership request from a previous run is still \
                     unresolved (unreachable: {err}); nothing new is planned until \
                     it resolves"
                )))
            }
            Err(other) => return Err(other),
        }

        let (input, status) = match self.observe().await? {
            Observed::Stop(report) => return Ok(*report),
            Observed::Ready(input, status) => (*input, *status),
        };
        let observation = ObservationFingerprint::of(&input);

        match placement::plan(&input) {
            PlacementRecommendation::NeedsProvisioning {
                candidates,
                desired_voters,
                retained_learners,
            } => {
                let evidence =
                    placement::selection_evidence(&input, &desired_voters, &retained_learners);
                let planned = PlannedPass {
                    observation,
                    desired_voters,
                    retained_learners,
                    evidence,
                };
                let mut provisioned: BTreeMap<CoordinationNodeId, MemberSeedV1> = BTreeMap::new();
                for node in candidates {
                    let request = ProvisionRequest {
                        group_id: self.client.group(),
                        policy_digest: input.policy.digest,
                        membership_log_id: input.membership_log_id.clone(),
                        node_id: node,
                    };
                    let seed = self.provisioner.provision(&request).await?;
                    if seed.node_id.as_slice() != node.as_bytes().as_slice() {
                        return Err(Error::InvalidArgument(format!(
                            "the provisioner answered for a different node than {}",
                            node.to_hex()
                        )));
                    }
                    provisioned.insert(node, seed);
                }
                self.recheck_and_submit(planned, provisioned).await
            }
            PlacementRecommendation::Plan(plan) => {
                let evidence = placement::selection_evidence(
                    &input,
                    &plan.desired_voters,
                    &plan.retained_learners,
                );
                let planned = PlannedPass {
                    observation,
                    desired_voters: plan.desired_voters.clone(),
                    retained_learners: plan.retained_learners.clone(),
                    evidence,
                };
                self.recheck_and_submit(planned, BTreeMap::new()).await
            }
            other => Ok(report_of(
                &self.client,
                &input.policy,
                &status,
                &input.grace_crossed_members,
                None,
                reason_of(&other),
                separation_of(&other),
            )),
        }
    }

    /// §7.7.3 step 5: re-read status, topology and policy, recompute, and
    /// submit only when the **fresh** observation still carries the exact
    /// `(policy revision/digest, membership LogId)` the plan — and any
    /// provisioning — was computed against, **and** the freshly recomputed
    /// selection is the retained one. A changed tuple or a changed selected
    /// topology fact discards the recommendation outright: the provisioned
    /// seeds were minted for the old pass and are not submitted into a new
    /// one, and no fresh plan is submitted on this pass — the next
    /// reconciliation starts over.
    async fn recheck_and_submit(
        &mut self,
        planned: PlannedPass,
        provisioned: BTreeMap<CoordinationNodeId, MemberSeedV1>,
    ) -> Result<PlacementReport> {
        let (mut input, status) = match self.observe().await? {
            Observed::Stop(report) => return Ok(*report),
            Observed::Ready(input, status) => (*input, *status),
        };
        let fresh = ObservationFingerprint::of(&input);
        if fresh != planned.observation {
            return Ok(report_of(
                &self.client,
                &input.policy,
                &status,
                &input.grace_crossed_members,
                None,
                "the policy revision/digest or membership log id changed between \
                 planning and submission; the plan and its provisioned seeds are \
                 discarded — the next reconciliation re-observes"
                    .to_string(),
                None,
            ));
        }
        // The tuple is unchanged, so the provisioned seeds join the candidate
        // material — only where the fresh policy still names the candidate
        // eligible and still lacks a seed.
        for (node, seed) in provisioned {
            if let Some(candidate) = input.policy.eligible.get_mut(&node) {
                if candidate.member_seed.is_none() {
                    candidate.member_seed = Some(seed);
                }
            }
        }
        match placement::plan(&input) {
            PlacementRecommendation::Plan(plan)
                if plan.desired_voters == planned.desired_voters
                    && plan.retained_learners == planned.retained_learners
                    // The evidence behind the retained selection, recomputed
                    // from the **fresh** snapshot: presence, co-clustering
                    // and pairwise separation verdicts must all be the facts
                    // the pass was planned on — identity equality alone
                    // would submit under moved evidence.
                    && placement::selection_evidence(
                        &input,
                        &planned.desired_voters,
                        &planned.retained_learners,
                    ) == planned.evidence =>
            {
                self.submit(&input.policy, &status, &input.grace_crossed_members, *plan)
                    .await
            }
            // §7.7.1: "any changed selected topology fact discards the
            // earlier recommendation." Same policy, same membership, but the
            // second snapshot selects different members — or the same
            // members under changed evidence — this pass submits nothing,
            // whatever the fresh plan says.
            PlacementRecommendation::Plan(_) => Ok(report_of(
                &self.client,
                &input.policy,
                &status,
                &input.grace_crossed_members,
                None,
                "the selected topology facts changed between planning and \
                 submission; the plan is discarded — the next reconciliation \
                 re-observes"
                    .to_string(),
                None,
            )),
            other => Ok(report_of(
                &self.client,
                &input.policy,
                &status,
                &input.grace_crossed_members,
                None,
                format!("the recheck superseded the plan: {}", reason_of(&other)),
                separation_of(&other),
            )),
        }
    }

    /// Submit through the durable journal. The operation id is the
    /// protocol's random 16 bytes — never the placement hash, never the
    /// provisioning key.
    async fn submit(
        &mut self,
        policy: &PlacementPolicy,
        status: &GroupStatusReplyWireV1,
        grace_crossed: &BTreeSet<CoordinationNodeId>,
        plan: PlacementPlan,
    ) -> Result<PlacementReport> {
        let mut operation_id = [0u8; 16];
        getrandom::getrandom(&mut operation_id)
            .map_err(|err| Error::InvalidArgument(format!("no operation id entropy: {err}")))?;
        let desired: Vec<NodeIdV1> = plan
            .desired_voters
            .iter()
            .map(|n| NodeIdV1 {
                node_id: n.as_bytes().to_vec(),
            })
            .collect();
        let learners: Vec<NodeIdV1> = plan
            .retained_learners
            .iter()
            .map(|n| NodeIdV1 {
                node_id: n.as_bytes().to_vec(),
            })
            .collect();
        let resolved: ResolvedMutation = self
            .client
            .change_membership(
                operation_id,
                plan.expected_membership_log_id.clone(),
                desired,
                learners,
                plan.member_seeds.clone(),
            )
            .await?;
        let reply = resolved.reply().clone();
        resolved.acknowledge().await?;
        let (pending, reason) = if reply.outcome_code == 8 {
            (
                Some(operation_id),
                "the plan was accepted; the controller is converging".to_string(),
            )
        } else {
            (
                None,
                format!(
                    "the submission lost the revision/intent race (outcome {}, reason {}); \
                     re-observe",
                    reply.outcome_code, reply.reason_code
                ),
            )
        };
        let mut report = report_of(
            &self.client,
            policy,
            status,
            grace_crossed,
            pending,
            reason,
            Some(plan.separation.clone()),
        );
        report.selected_voters = plan.desired_voters;
        report.selected_learners = plan.retained_learners;
        Ok(report)
    }

    /// One observation pass: status cut, snapshot, policy, grace timers.
    async fn observe(&mut self) -> Result<Observed> {
        let status = match self.client.reads().group_status().await {
            Ok(status) => status,
            // "No quorum produces a report only — topology cannot force
            // membership." The same goes for no-leader — whether the group
            // answered UNAVAILABLE or the routing walk exhausted every
            // member without an answer at all.
            Err(Error::Rpc {
                code: 14, message, ..
            }) => {
                return Ok(Observed::Stop(Box::new(self.stub_report(format!(
                    "no leader or no quorum: {message}; nothing is submitted"
                )))))
            }
            Err(Error::Transport(err)) => {
                return Ok(Observed::Stop(Box::new(self.stub_report(format!(
                    "no leader or no quorum reachable: {err}; nothing is submitted"
                )))))
            }
            Err(other) => return Err(other),
        };
        let voters = node_set(&status.voters)?;
        let learners = node_set(&status.learners)?;
        let policy = self.policy_source.policy()?;
        if status.membership_joint || status.pending_operation_id.is_some() {
            let pending = status
                .pending_operation_id
                .as_ref()
                .and_then(|id| <[u8; 16]>::try_from(id.as_slice()).ok());
            let mut report = report_of(
                &self.client,
                &policy,
                &status,
                // No topology pass ran, so no grace observation exists yet.
                &BTreeSet::new(),
                pending,
                "a membership operation is converging; no second plan exists".to_string(),
                None,
            );
            report.membership_log_id = status.membership_log_id.clone();
            return Ok(Observed::Stop(Box::new(report)));
        }
        let snapshot = self.topology.snapshot().await?;

        // Grace timers: current members only, first-absent instant kept
        // until the member reappears in a snapshot or leaves membership.
        let members: BTreeSet<CoordinationNodeId> = voters.union(&learners).copied().collect();
        self.absence.retain(|node, _| members.contains(node));
        let now = Instant::now();
        for member in &members {
            if snapshot.contains_hex(&member.to_hex()) {
                self.absence.remove(member);
            } else {
                self.absence.entry(*member).or_insert(now);
            }
        }
        let grace_crossed: BTreeSet<CoordinationNodeId> = self
            .absence
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= policy.replacement_grace)
            .map(|(node, _)| *node)
            .collect();

        // The genesis hash rides the same replicated cut as the membership:
        // it is the fourth binding every seed's join permit must verify
        // against before a plan may exist.
        let genesis_hash: [u8; 32] = status.genesis_hash.as_slice().try_into().map_err(|_| {
            Error::InvalidArgument(
                "the group status carries no 32-byte genesis hash; permits cannot be \
                 verified against this cut"
                    .into(),
            )
        })?;

        Ok(Observed::Ready(
            Box::new(PlacementInput {
                group_id: self.client.group(),
                genesis_hash,
                deployment_profile_code: status.deployment_profile_code,
                membership_log_id: status.membership_log_id.clone(),
                current_voters: voters,
                current_learners: learners,
                active_membership_intent: false,
                grace_crossed_members: grace_crossed,
                policy,
                topology: snapshot,
            }),
            Box::new(status),
        ))
    }

    /// A report for the paths where no status cut (or no plan) exists.
    fn stub_report(&self, reason: String) -> PlacementReport {
        PlacementReport {
            policy_revision: 0,
            policy_digest: [0; 32],
            membership_log_id: None,
            current_voters: Vec::new(),
            current_learners: Vec::new(),
            selected_voters: Vec::new(),
            selected_learners: Vec::new(),
            grace_crossed_members: Vec::new(),
            separation: None,
            pending_operation_id: None,
            reason,
        }
    }
}

fn node_set(nodes: &[NodeIdV1]) -> Result<BTreeSet<CoordinationNodeId>> {
    nodes
        .iter()
        .map(|n| {
            <[u8; 32]>::try_from(n.node_id.as_slice())
                .map_err(|_| Error::InvalidArgument("a member id is not 32 bytes".into()))
                .map(|bytes| CoordinationNodeId::from_slice(&bytes).expect("32 bytes is 32 bytes"))
        })
        .collect()
}

fn report_of(
    client: &CoordinationClient,
    policy: &PlacementPolicy,
    status: &GroupStatusReplyWireV1,
    grace_crossed: &BTreeSet<CoordinationNodeId>,
    pending: Option<[u8; 16]>,
    reason: String,
    separation: Option<placement::SeparationSummary>,
) -> PlacementReport {
    let _ = client;
    let as_ids = |nodes: &[NodeIdV1]| -> Vec<CoordinationNodeId> {
        nodes
            .iter()
            .filter_map(|n| CoordinationNodeId::from_slice(&n.node_id).ok())
            .collect()
    };
    PlacementReport {
        policy_revision: policy.revision,
        policy_digest: policy.digest,
        membership_log_id: status.membership_log_id.clone(),
        current_voters: as_ids(&status.voters),
        current_learners: as_ids(&status.learners),
        selected_voters: Vec::new(),
        selected_learners: Vec::new(),
        grace_crossed_members: grace_crossed.iter().copied().collect(),
        separation,
        pending_operation_id: pending,
        reason,
    }
}

fn reason_of(recommendation: &PlacementRecommendation) -> String {
    match recommendation {
        PlacementRecommendation::Stable { .. } => {
            "current roles already satisfy policy".to_string()
        }
        PlacementRecommendation::Degraded { why, .. } => format!("degraded: {why}"),
        PlacementRecommendation::Converging => "a membership operation is converging".to_string(),
        PlacementRecommendation::NeedsProvisioning { candidates, .. } => format!(
            "{} selected candidates lack seed material",
            candidates.len()
        ),
        PlacementRecommendation::InsufficientEvidence { missing } => {
            format!("insufficient evidence: {missing}")
        }
        PlacementRecommendation::InvalidPolicy { why } => format!("invalid policy: {why}"),
        PlacementRecommendation::Plan(_) => "a plan exists".to_string(),
    }
}

fn separation_of(recommendation: &PlacementRecommendation) -> Option<placement::SeparationSummary> {
    match recommendation {
        PlacementRecommendation::Stable { separation }
        | PlacementRecommendation::Degraded { separation, .. } => Some(separation.clone()),
        PlacementRecommendation::Plan(plan) => Some(plan.separation.clone()),
        _ => None,
    }
}
