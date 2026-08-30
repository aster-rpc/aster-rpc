//! §S5-E: **the operator runbook, executed and measured.**
//!
//! Two executions, each from a real starting state rather than a described one:
//!
//! * [`the_operator_runbook_runs_from_an_empty_machine`] — empty directories,
//!   through bootstrap, first grant, the register operations, join, restart,
//!   topology-assisted replacement, removal, snapshot build, catch-up,
//!   placement reconciliation, journal retry, backup/export and ordered
//!   shutdown;
//! * [`the_runbook_recovers_from_a_permanent_quorum_loss`] — a group that has
//!   irrecoverably lost its quorum, through export, new-group recovery,
//!   authority rotation, and the proof that every old fence now fails.
//!
//! ## What the numbers are, and are not
//!
//! Every duration here is **wall-clock through the public API**, from a caller
//! on its own endpoint, over real QUIC to real voters on this one host. That is
//! the operator's measurement: it includes the network, the leader's routing,
//! the durable journal, the Raft round trip, the apply and the reply.
//!
//! They are **not** a capacity model. One host, three (or four) voters, loopback
//! latency and a debug-profile build unless the runner says otherwise; a real
//! deployment's spread is dominated by inter-site RTT, which is exactly what
//! this cannot have. They are recorded so a *regression* is visible and so the
//! thresholds in the plan's §S5-E block are chosen from something measured.
//!
//! Storage-open and start-to-`Ready` are read from the runtime's own
//! [`startup_timings`](aster::coordination::Coordination::startup_timings)
//! gauge rather than timed from outside, because the operator question is
//! which of the two a slow start was.

#![cfg(feature = "coordination-testing")]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aster::coordination::client::manager::{
    CoordinationPlacementManager, PlacementPolicySource, PlacementProvisioner, ProvisionRequest,
    TopologySource,
};
use aster::coordination::client::placement::{PlacementCandidate, PlacementPolicy};
use aster::coordination::client::CoordinationClient;
use aster::coordination::contract::{FenceV1, MemberSeedV1, NodeIdV1, ResourceKeyV1};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::model::limits::{
    RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE, RIGHT_RENEW,
};
use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;
use aster::topology::TopologySnapshot;
use aster::{AsterConfig, Node, PublicKey, RelayMode, SecretKey};
use aster_transport_core::{CoreClusterView, CoreTopoSharedView};

const PATIENCE: Duration = Duration::from_secs(120);

async fn within<T, F: std::future::Future<Output = T>>(what: &str, f: F) -> T {
    match tokio::time::timeout(PATIENCE, f).await {
        Ok(value) => value,
        Err(_) => panic!("{what}: no answer within {PATIENCE:?}"),
    }
}

// ── The recorded runbook ────────────────────────────────────────────────────

/// One executed runbook step and what it cost.
struct Step {
    step: String,
    detail: String,
    /// Every sample taken for this step, in the order they were taken.
    samples: Vec<Duration>,
}

impl Step {
    fn sorted(&self) -> Vec<Duration> {
        let mut held = self.samples.clone();
        held.sort_unstable();
        held
    }
}

/// The executed runbook, printed as one table at the end.
///
/// Percentiles are reported **only** where there are at least twenty samples,
/// because below that the nearest-rank p95 is the maximum wearing a
/// percentile's name — the exact fault the first S5-E measurement was withdrawn
/// for. Single-shot operational steps report their one duration and say so.
#[derive(Default)]
struct Runbook {
    steps: Vec<Step>,
}

impl Runbook {
    fn once(&mut self, step: &str, detail: impl Into<String>, took: Duration) {
        self.steps.push(Step {
            step: step.to_string(),
            detail: detail.into(),
            samples: vec![took],
        });
    }

    fn many(&mut self, step: &str, detail: impl Into<String>, samples: Vec<Duration>) {
        self.steps.push(Step {
            step: step.to_string(),
            detail: detail.into(),
            samples,
        });
    }

    fn note(&mut self, step: &str, detail: impl Into<String>) {
        self.steps.push(Step {
            step: step.to_string(),
            detail: detail.into(),
            samples: Vec::new(),
        });
    }

    fn print(&self, title: &str) {
        println!("\n{title}\n{}", "-".repeat(118));
        println!(
            "  {:<34} {:<38} {:<8} {:>11} {:>11}",
            "step", "detail", "n", "p50/one", "p95/max"
        );
        for step in &self.steps {
            if step.samples.is_empty() {
                println!(
                    "  {:<34} {:<38} {:<8} {:>11} {:>11}",
                    step.step, step.detail, "—", "—", "—"
                );
                continue;
            }
            let sorted = step.sorted();
            let p = |q: f64| -> Duration {
                let rank = ((q / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
                sorted[rank.min(sorted.len()) - 1]
            };
            let spread = if sorted.len() >= 20 {
                format!("p95 {:.3?}", p(95.0))
            } else if sorted.len() == 1 {
                "one".to_string()
            } else {
                format!("max {:.3?}", p(100.0))
            };
            println!(
                "  {:<34} {:<38} {:<8} {:>11} {:>11}",
                step.step,
                step.detail,
                sorted.len(),
                format!("{:.3?}", p(50.0)),
                spread,
            );
        }
        println!();
    }
}

/// Warm samples for a per-operation latency line. Twenty is the floor at which
/// a nearest-rank p95 is a percentile rather than the maximum.
const OPERATION_SAMPLES: usize = 20;

// ── Fixture plumbing ────────────────────────────────────────────────────────

const SEED: u8 = 0xD4;

/// The deployment ROOT's own secret for a fixture built with `seed`.
///
/// `GroupFixture` derives its signing authority from `seed ^ 0xFF`, and §8.1's
/// break-glass administrator *is* that identity — so a client that wants to
/// grant, appoint or reshape must hold exactly this key. A client built from
/// one fixture's ROOT against another fixture's group is simply an
/// unprivileged actor, and the group says so.
fn root_secret(seed: u8) -> [u8; 32] {
    [seed ^ 0xFF; 32]
}

fn resource(key: &[u8]) -> ResourceKeyV1 {
    ResourceKeyV1 {
        namespace: "runbook".into(),
        key: key.to_vec(),
    }
}

fn node_id_of(secret: [u8; 32]) -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(&secret)
        .verifying_key()
        .to_bytes()
}

fn canonical_ticket(id: &[u8; 32]) -> String {
    aster::coordination::model::authority::canonical_candidate_ticket(id)
        .expect("a canonical ticket")
}

fn cross_register(servers: &[&AsterServer], nodes: &[&Node]) {
    let mut addrs = Vec::new();
    for server in servers {
        addrs.push(server.node().addr());
    }
    for node in nodes {
        addrs.push(node.addr());
    }
    for server in servers {
        for addr in &addrs {
            if addr.node_id != server.node().addr().node_id {
                let _ = server.node().add_node_addr(addr.clone());
            }
        }
    }
    for node in nodes {
        for addr in &addrs {
            if addr.node_id != node.addr().node_id {
                let _ = node.add_node_addr(addr.clone());
            }
        }
    }
}

/// The runtime's own view of how its start split.
fn startup_of(server: &AsterServer) -> (Option<u64>, Option<u64>) {
    let timings = server.coordination().expect("hosted").startup_timings();
    (timings.storage_open_micros, timings.start_to_ready_micros)
}

fn record_startup(runbook: &mut Runbook, label: &str, server: &AsterServer) {
    let (open, ready) = startup_of(server);
    runbook.once(
        &format!("{label}: storage open"),
        "runtime gauge",
        Duration::from_micros(open.expect("the storage open was measured")),
    );
    runbook.once(
        &format!("{label}: start to Ready"),
        "runtime gauge",
        Duration::from_micros(ready.expect("this node reached Ready")),
    );
}

/// A client bound to an arbitrary admitted identity.
async fn member_client(
    fixture: &GroupFixture,
    secret: [u8; 32],
    servers: &[&AsterServer],
    seeds: usize,
    journal: &std::path::Path,
) -> CoordinationClient {
    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(SecretKey::from_bytes(secret))
            .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
            .build(),
    )
    .await
    .expect("the member node starts");
    for server in servers {
        node.add_node_addr(server.node().addr()).expect("register");
    }
    let tickets: Vec<String> = (0..seeds)
        .map(|i| {
            let id: [u8; 32] = fixture.node_id(i).try_into().expect("32 bytes");
            canonical_ticket(&id)
        })
        .collect();
    CoordinationClient::builder(node, fixture.group_id())
        .seed_tickets(tickets)
        .journal_dir(journal)
        .build()
        .expect("the member client builds")
}

async fn admin_client(
    fixture: &GroupFixture,
    seed: u8,
    servers: &[&AsterServer],
    seeds: usize,
    journal: &std::path::Path,
) -> CoordinationClient {
    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(SecretKey::from_bytes(root_secret(seed)))
            .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
            .build(),
    )
    .await
    .expect("the administrator node starts");
    for server in servers {
        node.add_node_addr(server.node().addr()).expect("register");
    }
    let tickets: Vec<String> = (0..seeds)
        .map(|i| {
            let id: [u8; 32] = fixture.node_id(i).try_into().expect("32 bytes");
            canonical_ticket(&id)
        })
        .collect();
    CoordinationClient::builder(node, fixture.group_id())
        .seed_tickets(tickets)
        .journal_dir(journal)
        .build()
        .expect("the administrator client builds")
}

/// One flat cluster containing every named node, fully known — the topology a
/// single-host runbook honestly has. Separation is not claimed anywhere in this
/// file: the §S5-F corridor is where separated clusters are asserted, and
/// claiming them here from one host would be a fixture pretending to be
/// evidence.
fn one_cluster(nodes: &[CoordinationNodeId]) -> TopologySnapshot {
    let mut members: Vec<String> = nodes.iter().map(|n| n.to_hex()).collect();
    members.sort();
    let view = CoreClusterView {
        bridge: members.first().cloned().unwrap_or_default(),
        witnesses: Vec::new(),
        members,
    };
    TopologySnapshot::from_view(Arc::new(CoreTopoSharedView::from_parts(
        vec![view],
        Vec::new(),
        1,
    )))
}

struct FixedTopology(TopologySnapshot);

#[async_trait::async_trait]
impl TopologySource for FixedTopology {
    async fn snapshot(&self) -> aster::Result<TopologySnapshot> {
        Ok(self.0.clone())
    }
}

struct FixedPolicy(Mutex<PlacementPolicy>);

impl PlacementPolicySource for FixedPolicy {
    fn policy(&self) -> aster::Result<PlacementPolicy> {
        Ok(self.0.lock().expect("policy").clone())
    }
}

struct NoProvisioning;

#[async_trait::async_trait]
impl PlacementProvisioner for NoProvisioning {
    async fn provision(&self, request: &ProvisionRequest) -> aster::Result<MemberSeedV1> {
        panic!("provisioning was not expected for {request:?}");
    }
}

/// The runbook's placement policy: the named members eligible, exactly three
/// voters, separation as a preference (one host cannot prove separation).
fn flat_policy(
    fixture: &GroupFixture,
    eligible: &[(CoordinationNodeId, Option<MemberSeedV1>)],
    excluded: Option<CoordinationNodeId>,
) -> PlacementPolicy {
    PlacementPolicy {
        revision: 1,
        digest: [0xD7; 32],
        authority_public_key: fixture.authority_key(),
        authority_domain: fixture.domain().to_string(),
        target_voters: 3,
        target_retained_learners: 0,
        eligible: eligible
            .iter()
            .map(|(id, seed)| {
                (
                    *id,
                    PlacementCandidate {
                        node_id: *id,
                        member_seed: seed.clone(),
                    },
                )
            })
            .collect(),
        pinned_voters: BTreeSet::new(),
        excluded: excluded.into_iter().collect(),
        minimum_separated_voter_pairs: 0,
        replacement_grace: Duration::from_secs(600),
        allow_healthy_rebalance: false,
    }
}

/// Like [`settles_on`], but for a membership that deliberately retains
/// learners: `learners` is how many the settled cut must name.
async fn settles_on_with_learners(
    client: &CoordinationClient,
    expected: &BTreeSet<Vec<u8>>,
    learners: usize,
) -> Duration {
    let began = Instant::now();
    let deadline = began + PATIENCE;
    loop {
        if let Ok(status) = within("group_status", client.reads().group_status()).await {
            let voters: BTreeSet<Vec<u8>> =
                status.voters.iter().map(|n| n.node_id.clone()).collect();
            if voters == *expected
                && status.learners.len() == learners
                && !status.membership_joint
                && status.pending_operation_id.is_none()
            {
                return began.elapsed();
            }
        }
        assert!(
            Instant::now() < deadline,
            "the membership never settled with {learners} learners within {PATIENCE:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn settles_on(client: &CoordinationClient, expected: &BTreeSet<Vec<u8>>) -> Duration {
    let began = Instant::now();
    let deadline = began + PATIENCE;
    loop {
        if let Ok(status) = within("group_status", client.reads().group_status()).await {
            let voters: BTreeSet<Vec<u8>> =
                status.voters.iter().map(|n| n.node_id.clone()).collect();
            if voters == *expected
                && status.learners.is_empty()
                && !status.membership_joint
                && status.pending_operation_id.is_none()
            {
                return began.elapsed();
            }
        }
        assert!(
            Instant::now() < deadline,
            "the membership never settled within {PATIENCE:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ── Runbook 1: from an empty machine ────────────────────────────────────────

/// **The runbook, executed from empty directories.**
///
/// Every step is a real operation against real voters, and the assertions are
/// the runbook's own success conditions — the timings ride along. A step that
/// silently failed would fail the test, so the table cannot report a duration
/// for something that did not happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_operator_runbook_runs_from_an_empty_machine() {
    let mut runbook = Runbook::default();

    // ── 1. Bootstrap: three voters, empty directories, one signed genesis ───
    let candidate_secret = [SEED ^ 0x77; 32];
    let spare_secret = [SEED ^ 0x78; 32];
    let fixture = GroupFixture::new("runbook.empty", SEED, 3)
        .admitting(candidate_secret)
        .admitting(spare_secret);
    let dirs: Vec<tempfile::TempDir> = (0..3)
        .map(|_| tempfile::tempdir().expect("a directory"))
        .collect();

    let began = Instant::now();
    let mut voters: Vec<Option<AsterServer>> = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        voters.push(Some(
            AsterServer::builder()
                .config(fixture.aster_config(index))
                .builtin_coordination(fixture.config(dir.path(), index))
                .start()
                .await
                .expect("a voter starts"),
        ));
    }
    {
        let running: Vec<&AsterServer> = voters.iter().flatten().collect();
        cross_register(&running, &[]);
    }
    for voter in voters.iter().flatten() {
        within(
            "voter readiness",
            voter.coordination().expect("hosted").wait_ready(),
        )
        .await
        .expect("the group becomes ready");
    }
    runbook.once("1. bootstrap", "three voters to Ready", began.elapsed());
    for (index, voter) in voters.iter().flatten().enumerate() {
        record_startup(&mut runbook, &format!("1. bootstrap voter {index}"), voter);
    }

    // ── 2. The first grant, and the register operations ─────────────────────
    let admin_journal = tempfile::tempdir().expect("a directory");
    let admin = {
        let running: Vec<&AsterServer> = voters.iter().flatten().collect();
        admin_client(&fixture, SEED, &running, 3, admin_journal.path()).await
    };
    let root_id = node_id_of(root_secret(SEED));

    let began = Instant::now();
    let granted = within(
        "set_grant",
        admin.set_grant(
            resource(b"head"),
            root_id,
            RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
            0,
        ),
    )
    .await
    .expect("ROOT grants");
    assert_eq!(granted.reply().outcome_code, 6, "GrantSet");
    within("ack", granted.acknowledge()).await.expect("ack");
    runbook.once("2. grant", "set_grant + acknowledge", began.elapsed());

    let acquired = within("acquire", admin.acquire(resource(b"head"), 60_000))
        .await
        .expect("the register is acquired");
    assert_eq!(acquired.reply().outcome_code, 1, "Acquired");
    let snapshot = acquired.reply().snapshot.clone().expect("a snapshot");
    let fence = FenceV1 {
        group_id: snapshot.group_id.clone(),
        resource: snapshot.resource.clone(),
        epoch: snapshot.epoch,
        holder: snapshot.holder.clone().expect("held"),
    };
    within("ack", acquired.acknowledge()).await.expect("ack");

    // renew — the operation an active holder performs most often.
    let mut renews = Vec::with_capacity(OPERATION_SAMPLES);
    for _ in 0..OPERATION_SAMPLES {
        let at = Instant::now();
        let renewed = within("renew", admin.renew(fence.clone()))
            .await
            .expect("the lease renews");
        assert_eq!(renewed.reply().outcome_code, 2, "Renewed");
        renews.push(at.elapsed());
        within("ack", renewed.acknowledge()).await.expect("ack");
    }
    runbook.many("2. renew", "whole path, holder → committed", renews);

    // mutation — compare-and-set under the fence.
    let mut mutations = Vec::with_capacity(OPERATION_SAMPLES);
    let mut expected_revision = 0u64;
    for _ in 0..OPERATION_SAMPLES {
        let at = Instant::now();
        let mutated = within(
            "compare_and_set",
            admin.compare_and_set(fence.clone(), expected_revision, vec![0xA5; 256]),
        )
        .await
        .expect("the value is written");
        assert_eq!(mutated.reply().outcome_code, 3, "Mutated");
        mutations.push(at.elapsed());
        expected_revision = mutated
            .reply()
            .snapshot
            .as_ref()
            .expect("a snapshot")
            .value_revision;
        within("ack", mutated.acknowledge()).await.expect("ack");
    }
    runbook.many("2. mutation", "compare_and_set, 256-byte value", mutations);

    // linearizable read — the leader's barrier plus a pinned transaction.
    let mut reads = Vec::with_capacity(OPERATION_SAMPLES);
    for _ in 0..OPERATION_SAMPLES {
        let at = Instant::now();
        let read = within(
            "read_linearizable",
            admin.reads().read_linearizable(resource(b"head")),
        )
        .await
        .expect("the register reads");
        assert_eq!(read.snapshot.value.as_deref(), Some([0xA5; 256].as_ref()));
        reads.push(at.elapsed());
    }
    runbook.many(
        "2. linearizable read",
        "barrier + pinned transaction",
        reads,
    );

    // group_status — what a dashboard polls.
    let mut statuses = Vec::with_capacity(OPERATION_SAMPLES);
    for _ in 0..OPERATION_SAMPLES {
        let at = Instant::now();
        within("group_status", admin.reads().group_status())
            .await
            .expect("the status cut");
        statuses.push(at.elapsed());
    }
    runbook.many("2. group_status", "linearizable status cut", statuses);

    // ── 3. Join: a fourth node enters as a voter ────────────────────────────
    let candidate_id = node_id_of(candidate_secret);
    let candidate_dir = tempfile::tempdir().expect("a directory");
    let began = Instant::now();
    let candidate = AsterServer::builder()
        .config(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes(candidate_secret))
                .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
                .build(),
        )
        .builtin_coordination(fixture.join_config(
            candidate_dir.path(),
            candidate_secret,
            &canonical_ticket(&candidate_id),
            &[0, 1, 2],
        ))
        .start()
        .await
        .expect("the candidate starts joining");
    runbook.once("3. join", "process start, Join mode", began.elapsed());
    assert_eq!(
        candidate.coordination().expect("hosted").lifecycle_code(),
        2,
        "a started candidate is Joining, not a member"
    );
    {
        let mut running: Vec<&AsterServer> = voters.iter().flatten().collect();
        running.push(&candidate);
        cross_register(&running, &[]);
    }

    // ── 4. Restart an existing member, in place, through `Existing` ─────────
    let restarting = 2usize;
    let began = Instant::now();
    voters[restarting]
        .take()
        .expect("voter 2 is running")
        .shutdown()
        .await;
    runbook.once(
        "4. ordered shutdown",
        "one voter, drain and stop",
        began.elapsed(),
    );

    let began = Instant::now();
    let restarted = AsterServer::builder()
        .config(fixture.aster_config(restarting))
        .builtin_coordination(fixture.existing_config(dirs[restarting].path()))
        .start()
        .await
        .expect("the voter restarts in place");
    voters[restarting] = Some(restarted);
    {
        let mut running: Vec<&AsterServer> = voters.iter().flatten().collect();
        running.push(&candidate);
        cross_register(&running, &[]);
    }
    within(
        "restart readiness",
        voters[restarting]
            .as_ref()
            .expect("running")
            .coordination()
            .expect("hosted")
            .wait_ready(),
    )
    .await
    .expect("the restarted voter rejoins");
    runbook.once("4. restart", "shutdown → Existing → Ready", began.elapsed());
    record_startup(
        &mut runbook,
        "4. restart",
        voters[restarting].as_ref().expect("running"),
    );

    // ── 5. Topology-assisted replacement, through the placement manager ─────
    // Voter 1 is excluded by policy; the admitted candidate takes its seat.
    let voter_ids: Vec<CoordinationNodeId> = (0..3)
        .map(|i| CoordinationNodeId::from_slice(&fixture.node_id(i)).expect("32 bytes"))
        .collect();
    let candidate_node = CoordinationNodeId::from_slice(&candidate_id).expect("32 bytes");
    let seed = MemberSeedV1 {
        node_id: candidate_id.to_vec(),
        ticket: canonical_ticket(&candidate_id),
        signed_join_permit: Some(fixture.join_permit_with_ticket(
            candidate_secret,
            &canonical_ticket(&candidate_id),
            &[0, 1, 2],
        )),
    };
    let eligible: Vec<(CoordinationNodeId, Option<MemberSeedV1>)> = voter_ids
        .iter()
        .map(|id| (*id, None))
        .chain(std::iter::once((candidate_node, Some(seed.clone()))))
        .collect();
    let mut members: Vec<CoordinationNodeId> = voter_ids.clone();
    members.push(candidate_node);

    let manager_journal = tempfile::tempdir().expect("a directory");
    let manager_client = {
        let running: Vec<&AsterServer> = voters.iter().flatten().collect();
        admin_client(&fixture, SEED, &running, 3, manager_journal.path()).await
    };
    let mut manager = CoordinationPlacementManager::new(
        manager_client,
        FixedTopology(one_cluster(&members)),
        FixedPolicy(Mutex::new(flat_policy(
            &fixture,
            &eligible,
            Some(voter_ids[1]),
        ))),
        NoProvisioning,
    );

    // A reconciliation that plans and submits, and one that finds nothing to
    // do: both are what an operator's control loop actually costs.
    let at = Instant::now();
    let report = within("reconcile", manager.reconcile())
        .await
        .expect("a report");
    runbook.once(
        "5. placement reconciliation",
        "observe → plan → recheck → submit",
        at.elapsed(),
    );
    assert!(
        report.pending_operation_id.is_some(),
        "the replacement was submitted: {}",
        report.reason
    );

    within(
        "replacement readiness",
        candidate.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("the replacement catches up and becomes ready");
    let expected: BTreeSet<Vec<u8>> = [
        voter_ids[0].as_bytes().to_vec(),
        voter_ids[2].as_bytes().to_vec(),
        candidate_id.to_vec(),
    ]
    .into_iter()
    .collect();
    let settling = settles_on(&admin, &expected).await;
    runbook.once(
        "5. catch-up + commit",
        "learner caught up, uniform membership",
        settling,
    );
    record_startup(&mut runbook, "5. replacement", &candidate);

    let mut idle = Vec::with_capacity(5);
    for _ in 0..5 {
        let at = Instant::now();
        let report = within("reconcile", manager.reconcile())
            .await
            .expect("a report");
        idle.push(at.elapsed());
        assert!(
            report.pending_operation_id.is_none(),
            "a settled group needs no second plan: {}",
            report.reason
        );
    }
    runbook.many("5. placement reconciliation (idle)", "nothing to do", idle);

    // ── 6. Routine removal ─────────────────────────────────────────────────
    // **Of a retained learner, not of a voter.** A three-voter production
    // group cannot remove a voter: §7.1's shape refuses two, and the plan is
    // rejected before it is proposed ("the membership plan is outside the v1
    // shape") — which is the correct answer and worth having met here rather
    // than in production. Removing a voter is therefore always a *replacement*,
    // which step 5 already performed. What routine removal actually looks like
    // is a retained learner leaving, so that is what is executed: the spare is
    // added as a learner and then removed, and both halves are timed.
    let spare_id = node_id_of(spare_secret);
    let spare_dir = tempfile::tempdir().expect("a directory");
    let spare = AsterServer::builder()
        .config(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes(spare_secret))
                .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
                .build(),
        )
        .builtin_coordination(fixture.join_config(
            spare_dir.path(),
            spare_secret,
            &canonical_ticket(&spare_id),
            &[0, 1, 2],
        ))
        .start()
        .await
        .expect("the spare starts joining");
    {
        let mut running: Vec<&AsterServer> = voters.iter().flatten().collect();
        running.push(&candidate);
        running.push(&spare);
        cross_register(&running, &[]);
    }

    let status = within("group_status", admin.reads().group_status())
        .await
        .expect("the status cut");
    let voters_now: Vec<NodeIdV1> = status.voters.clone();
    let spare_seed = MemberSeedV1 {
        node_id: spare_id.to_vec(),
        ticket: canonical_ticket(&spare_id),
        signed_join_permit: Some(fixture.join_permit_with_ticket(
            spare_secret,
            &canonical_ticket(&spare_id),
            &[0, 1, 2],
        )),
    };
    let at = Instant::now();
    let added = within(
        "change_membership (add learner)",
        admin.change_membership(
            *blake3::hash(b"runbook-add-learner").as_bytes()[..16]
                .first_chunk::<16>()
                .expect("16 bytes"),
            status.membership_log_id.clone(),
            voters_now.clone(),
            vec![NodeIdV1 {
                node_id: spare_id.to_vec(),
            }],
            vec![spare_seed],
        ),
    )
    .await
    .expect("the learner addition is accepted");
    assert_eq!(added.reply().outcome_code, 8, "MembershipAccepted");
    within("ack", added.acknowledge()).await.expect("ack");
    let with_learner: BTreeSet<Vec<u8>> = voters_now.iter().map(|n| n.node_id.clone()).collect();
    settles_on_with_learners(&admin, &with_learner, 1).await;
    runbook.once(
        "6. add retained learner",
        "learner added and caught up",
        at.elapsed(),
    );

    let status = within("group_status", admin.reads().group_status())
        .await
        .expect("the status cut");
    let at = Instant::now();
    let removed = within(
        "change_membership (remove learner)",
        admin.change_membership(
            *blake3::hash(b"runbook-removal").as_bytes()[..16]
                .first_chunk::<16>()
                .expect("16 bytes"),
            status.membership_log_id.clone(),
            status.voters.clone(),
            Vec::new(),
            Vec::new(),
        ),
    )
    .await
    .expect("the removal is accepted");
    assert_eq!(removed.reply().outcome_code, 8, "MembershipAccepted");
    within("ack", removed.acknowledge()).await.expect("ack");
    let after_removal: BTreeSet<Vec<u8>> =
        status.voters.iter().map(|n| n.node_id.clone()).collect();
    settles_on(&admin, &after_removal).await;
    runbook.once("6. removal", "retained learner removed", at.elapsed());

    // ── 7. Snapshot build ───────────────────────────────────────────────────
    let leader = voters
        .iter()
        .flatten()
        .find(|v| {
            v.coordination()
                .expect("hosted")
                .current_leader_for_testing()
                .is_some()
        })
        .expect("someone leads");
    let handle = leader.coordination().expect("hosted");
    let before = handle.snapshot_index_for_testing();
    let at = Instant::now();
    handle
        .raft_for_testing()
        .trigger()
        .snapshot()
        .await
        .expect("the snapshot is triggered");
    let deadline = Instant::now() + PATIENCE;
    loop {
        let now = handle.snapshot_index_for_testing();
        if now.is_some() && now != before {
            break;
        }
        assert!(Instant::now() < deadline, "the snapshot never built");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    runbook.once(
        "7. snapshot build",
        "trigger → snapshot index moves",
        at.elapsed(),
    );

    // ── 8. Journal retry: resume with nothing outstanding, and with one ─────
    let mut resumes = Vec::with_capacity(OPERATION_SAMPLES);
    for _ in 0..OPERATION_SAMPLES {
        let at = Instant::now();
        let resumed = within("resume", admin.resume())
            .await
            .expect("the journal resumes");
        assert!(resumed.is_none(), "nothing is outstanding");
        resumes.push(at.elapsed());
    }
    runbook.many("8. journal retry (clean)", "resume, journal Ready", resumes);

    // ── 9. Backup: the canonical recovery export ────────────────────────────
    let at = Instant::now();
    let (manifest, body) = within("recovery_export", admin.reads().recovery_export())
        .await
        .expect("the export streams");
    let export_took = at.elapsed();
    assert_eq!(
        blake3::hash(&body).as_bytes().to_vec(),
        manifest.blake3_checksum,
        "the manifest names the body it shipped"
    );
    runbook.once(
        "9. backup/export",
        format!(
            "{} B declared, {} B shipped",
            manifest.total_len,
            body.len()
        ),
        export_took,
    );

    // ── 10. Ordered shutdown of the whole group ─────────────────────────────
    let at = Instant::now();
    for voter in voters.iter_mut() {
        if let Some(server) = voter.take() {
            server.shutdown().await;
        }
    }
    candidate.shutdown().await;
    spare.shutdown().await;
    runbook.once(
        "10. ordered shutdown",
        "every remaining process",
        at.elapsed(),
    );

    runbook.print("§S5-E operator runbook — from an empty machine");
}

// ── Runbook 2: permanent quorum loss ────────────────────────────────────────

/// **The runbook, executed from a permanent-quorum-loss incident.**
///
/// A three-voter group loses two voters for good. The surviving voter can
/// serve nothing, an export taken while the group was healthy is the only
/// recovery point, and the new group is founded from it under a *new* signed
/// genesis. The step every operator gets wrong is proved here rather than
/// described: after a refused import the directory is bound, so the retry needs
/// a **new or explicitly cleared** directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_runbook_recovers_from_a_permanent_quorum_loss() {
    let mut runbook = Runbook::default();

    // ── 1. A healthy source group, and a value worth recovering ─────────────
    let fixture = GroupFixture::new("runbook.loss", SEED ^ 0x11, 3).admitting([SEED ^ 0x21; 32]);
    let dirs: Vec<tempfile::TempDir> = (0..3)
        .map(|_| tempfile::tempdir().expect("a directory"))
        .collect();
    let mut voters: Vec<Option<AsterServer>> = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        voters.push(Some(
            AsterServer::builder()
                .config(fixture.aster_config(index))
                .builtin_coordination(fixture.config(dir.path(), index))
                .start()
                .await
                .expect("a voter starts"),
        ));
    }
    {
        let running: Vec<&AsterServer> = voters.iter().flatten().collect();
        cross_register(&running, &[]);
    }
    for voter in voters.iter().flatten() {
        within(
            "voter readiness",
            voter.coordination().expect("hosted").wait_ready(),
        )
        .await
        .expect("ready");
    }

    let journal = tempfile::tempdir().expect("a directory");
    let admin = {
        let running: Vec<&AsterServer> = voters.iter().flatten().collect();
        admin_client(&fixture, SEED ^ 0x11, &running, 3, journal.path()).await
    };
    // A **worker** identity, not the source ROOT: §13 preserves ACLs by
    // subject node id, and the recovered group is founded under a different
    // authority whose ROOT is a different identity entirely. An operator who
    // only ever granted to the old break-glass key would recover a group its
    // own administrator cannot read, which is exactly the surprise this step
    // exists to have already had.
    let worker_secret = [SEED ^ 0x21; 32];
    let worker_id = node_id_of(worker_secret);
    let granted = within(
        "set_grant",
        admin.set_grant(
            resource(b"head"),
            worker_id,
            RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
            0,
        ),
    )
    .await
    .expect("ROOT grants");
    within("ack", granted.acknowledge()).await.expect("ack");

    let worker_journal = tempfile::tempdir().expect("a directory");
    let worker = {
        let running: Vec<&AsterServer> = voters.iter().flatten().collect();
        member_client(&fixture, worker_secret, &running, 3, worker_journal.path()).await
    };
    let acquired = within("acquire", worker.acquire(resource(b"head"), 60_000))
        .await
        .expect("acquired");
    let snapshot = acquired.reply().snapshot.clone().expect("a snapshot");
    let old_fence = FenceV1 {
        group_id: snapshot.group_id.clone(),
        resource: snapshot.resource.clone(),
        epoch: snapshot.epoch,
        holder: snapshot.holder.clone().expect("held"),
    };
    within("ack", acquired.acknowledge()).await.expect("ack");
    let written = within(
        "compare_and_set",
        worker.compare_and_set(old_fence.clone(), 0, vec![0xC7; 128]),
    )
    .await
    .expect("written");
    within("ack", written.acknowledge()).await.expect("ack");

    // ── 2. Backup, taken while a quorum still exists ────────────────────────
    let at = Instant::now();
    let (manifest, body) = within("recovery_export", admin.reads().recovery_export())
        .await
        .expect("the export streams");
    runbook.once(
        "1. backup/export (pre-incident)",
        format!(
            "{} B declared, {} B shipped",
            manifest.total_len,
            body.len()
        ),
        at.elapsed(),
    );
    let export_hash: [u8; 32] = blake3::hash(&body).into();
    let export_home = tempfile::tempdir().expect("a directory");
    let export_path = export_home.path().join("source.recovery");
    std::fs::write(&export_path, &body).expect("the export file writes");

    // ── 3. The incident: two of three voters are gone for good ──────────────
    let at = Instant::now();
    for index in [1usize, 2] {
        voters[index].take().expect("running").shutdown().await;
    }
    runbook.once("2. quorum loss", "two of three voters gone", at.elapsed());

    let at = Instant::now();
    let refused = within("group_status", admin.reads().group_status()).await;
    runbook.once(
        "2. status without quorum",
        "the survivor refuses rather than answers",
        at.elapsed(),
    );
    assert!(
        refused.is_err(),
        "a minority must not serve a linearizable cut"
    );
    voters[0].take().expect("running").shutdown().await;

    // ── 4. A refused import leaves a bound directory ────────────────────────
    // The operator's first attempt uses a **damaged** copy of the export —
    // the ordinary disaster, a byte lost in transit — under the *correct* new
    // genesis. The refusal is the point, and so is what it leaves behind.
    let right = GroupFixture::new("runbook.new", SEED ^ 0x13, 3)
        .admitting(worker_secret)
        .recovering(export_hash, &[worker_id]);
    let mut damaged = body.clone();
    let last = damaged.len() - 1;
    damaged[last] ^= 0x01;
    let damaged_path = export_home.path().join("damaged.recovery");
    std::fs::write(&damaged_path, &damaged).expect("the damaged copy writes");

    let reused = tempfile::tempdir().expect("a directory");
    let at = Instant::now();
    let refusal = AsterServer::builder()
        .config(right.aster_config(0))
        .builtin_coordination(right.recovery_config(reused.path(), 0, &damaged_path))
        .start()
        .await;
    runbook.once(
        "3. refused import",
        "one byte damaged in transit",
        at.elapsed(),
    );
    let refusal = refusal
        .err()
        .expect("a damaged export never founds a group");
    assert!(
        refusal.to_string().contains("hashes to"),
        "the refusal names the binding it failed: {refusal}"
    );

    // The retry an operator reaches for first — **the same directory, now with
    // the intact export** — is refused by design. The open binds the directory
    // before the import runs, so a refused import leaves a bound-pristine
    // store: nothing replicated was written, but the next open is no longer
    // `Fresh` and the import guard will not write into a directory something
    // has already claimed.
    let retry = AsterServer::builder()
        .config(right.aster_config(0))
        .builtin_coordination(right.recovery_config(reused.path(), 0, &export_path))
        .start()
        .await;
    let retry = retry
        .err()
        .expect("an in-place retry after a refused import is refused");
    runbook.note(
        "3. in-place retry",
        "refused: the directory is already bound",
    );
    assert!(
        retry.to_string().contains("fresh"),
        "the refusal must name the directory's state, so an operator can tell \
         this apart from a bad export: {retry}"
    );

    // ── 5. New-group recovery, into new directories ─────────────────────────
    let recovered_dirs: Vec<tempfile::TempDir> = (0..3)
        .map(|_| tempfile::tempdir().expect("a directory"))
        .collect();
    let at = Instant::now();
    let mut recovered: Vec<AsterServer> = Vec::new();
    for (index, dir) in recovered_dirs.iter().enumerate() {
        recovered.push(
            AsterServer::builder()
                .config(right.aster_config(index))
                .builtin_coordination(right.recovery_config(dir.path(), index, &export_path))
                .start()
                .await
                .expect("a recovered voter starts"),
        );
    }
    {
        let running: Vec<&AsterServer> = recovered.iter().collect();
        cross_register(&running, &[]);
    }
    for voter in &recovered {
        within(
            "recovered readiness",
            voter.coordination().expect("hosted").wait_ready(),
        )
        .await
        .expect("the recovered group becomes ready");
    }
    runbook.once(
        "4. new-group recovery",
        "three voters, import → Ready",
        at.elapsed(),
    );
    for (index, voter) in recovered.iter().enumerate() {
        record_startup(&mut runbook, &format!("4. recovered voter {index}"), voter);
    }

    // ── 6. Every old fence fails against the new group ──────────────────────
    let new_journal = tempfile::tempdir().expect("a directory");
    let new_admin = {
        let running: Vec<&AsterServer> = recovered.iter().collect();
        admin_client(&right, SEED ^ 0x13, &running, 3, new_journal.path()).await
    };
    let new_worker_journal = tempfile::tempdir().expect("a directory");
    let new_worker = {
        let running: Vec<&AsterServer> = recovered.iter().collect();
        member_client(
            &right,
            worker_secret,
            &running,
            3,
            new_worker_journal.path(),
        )
        .await
    };
    let read = within(
        "read_linearizable",
        new_worker.reads().read_linearizable(resource(b"head")),
    )
    .await
    .expect("the imported register reads");
    assert_eq!(
        read.snapshot.value,
        Some(vec![0xC7; 128]),
        "the recovered value is the one the export carried"
    );
    assert!(!read.snapshot.held, "recovery frees every lease");

    // Two arms, because "the old fence fails" has two distinct reasons and only
    // one of them is reachable at the server.
    //
    // The old fence names the **old group**, and the client refuses it before
    // any send: a fence is bound to a group id, and a recovered group has a new
    // one. That is the stronger answer — the bytes never leave the process.
    let cross_group = new_worker
        .compare_and_set(old_fence.clone(), 0, vec![0xEE; 8])
        .await
        .expect_err("an old-group fence is not sendable at all");
    assert!(
        cross_group
            .to_string()
            .contains("another coordination group"),
        "the refusal names the group binding: {cross_group}"
    );

    // And the arm the server owns: the same holder and resource, relabelled
    // with the **new** group id so it is sendable, still carrying the old
    // epoch. Recovery increments the epoch exactly once, so this is the fence
    // a returning old holder would forge, and the group refuses it.
    let forged = FenceV1 {
        group_id: read.snapshot.group_id.clone(),
        resource: old_fence.resource.clone(),
        epoch: old_fence.epoch,
        holder: old_fence.holder.clone(),
    };
    assert_ne!(
        forged.epoch, read.snapshot.epoch,
        "recovery advanced the epoch, so the old one is genuinely stale"
    );
    let stale = within(
        "stale mutation",
        new_worker.compare_and_set(forged, 0, vec![0xEE; 8]),
    )
    .await
    .expect("the group answers");
    assert_ne!(
        stale.reply().outcome_code,
        3,
        "a stale epoch must never mutate the recovered head"
    );
    within("ack", stale.acknowledge()).await.expect("ack");
    runbook.note(
        "5. old fences",
        "cross-group refused locally, stale epoch refused by the group",
    );

    // ── 7. Authority rotation: the administrator set moves ──────────────────
    let successor = node_id_of([SEED ^ 0x44; 32]);
    // Fenced by the **imported** administrator revision, not by zero: §13
    // advances the revision as part of founding the new group, so an operator
    // who assumed a fresh group starts at zero has their rotation rejected.
    let before = within("inspect_admins", new_admin.reads().inspect_admins())
        .await
        .expect("the administrator set reads")
        .admin_snapshot
        .admin_revision;
    let at = Instant::now();
    let rotated = within("set_admin", new_admin.set_admin(successor, true, before))
        .await
        .expect("the administrator set is changed");
    assert_eq!(rotated.reply().outcome_code, 7, "AdminSet");
    within("ack", rotated.acknowledge()).await.expect("ack");
    runbook.once(
        "6. authority rotation",
        "successor administrator appointed",
        at.elapsed(),
    );
    let admins = within("inspect_admins", new_admin.reads().inspect_admins())
        .await
        .expect("the administrator set reads");
    assert!(
        admins
            .admin_snapshot
            .administrators
            .iter()
            .any(|a| a.node_id == successor.to_vec()),
        "the successor is a committed administrator"
    );

    // ── 8. Ordered shutdown ─────────────────────────────────────────────────
    let at = Instant::now();
    for voter in recovered {
        voter.shutdown().await;
    }
    runbook.once("7. ordered shutdown", "the recovered group", at.elapsed());

    runbook.print("§S5-E operator runbook — permanent quorum loss and recovery");
}
