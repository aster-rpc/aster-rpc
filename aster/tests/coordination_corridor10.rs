//! §S5-F: the **ten-producer / three-voter placement corridor**.
//!
//! Ten real admitted producers, each an Aster node with an ephemeral port
//! reached by NodeId, arranged by controlled topology evidence into three
//! positively separated clusters of **4 / 3 / 3**, under an explicit
//! eligible-host record, `minimum_separated_voter_pairs = 3`, and exactly three
//! voters.
//!
//! ```text
//!   cluster A (4)          cluster B (3)        cluster C (3)
//!   v0  v1  v2  c3         c4  c5  p7           c6  p8  p9
//!   ^^^^^^^^^^^^ the genesis voters all start here, deliberately
//! ```
//!
//! The genesis puts all three voters in one cluster on purpose. A corridor
//! whose voters are already one-per-cluster cannot demonstrate that placement
//! *selects* anything — the planner would report `Stable` and the assertion
//! would be about the fixture. Starting undiverse makes the first pass a real
//! selection, and it is the same shape the pure §15 corridor case uses.
//!
//! `c3`–`c6` are `Join`-mode coordination hosts: admitted, reachable, and
//! deliberately not members, so a plan that names one is followed by a real
//! learner catch-up and promotion. `p7`–`p9` are plain admitted producer nodes
//! that the eligible record does not name — the corridor's standing proof that
//! admission and liveness are not eligibility.
//!
//! Topology is a deterministic [`TopologySnapshot`] built from explicit parts:
//! the same view type and the same coverage rule production derives, without
//! running a swarm. That is what "controlled topology evidence" means here —
//! the evidence is chosen, so the assertion is about the planner rather than
//! about what a live swarm happened to measure.
//!
//! Every reconciliation's report, every membership cut, the policy, the
//! snapshots and the client journal are retained as failure artifacts and
//! written to `target/s5f-artifacts/<test>/` when a test panics.

#![cfg(feature = "coordination-testing")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::coordination::client::manager::{
    CoordinationPlacementManager, PlacementPolicySource, PlacementProvisioner, ProvisionRequest,
    TopologySource,
};
use aster::coordination::client::placement::{
    plan, PlacementCandidate, PlacementInput, PlacementPolicy, PlacementRecommendation,
    PlacementReport,
};
use aster::coordination::client::CoordinationClient;
use aster::coordination::contract::MemberSeedV1;
use aster::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
use aster::coordination::model::limits::{
    RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE, RIGHT_RENEW,
};
use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;
use aster::topology::TopologySnapshot;
use aster::{AsterConfig, Node, PublicKey, RelayMode, SecretKey};
use aster_transport_core::{CoreClusterView, CoreTopoSharedView};

const SEED: u8 = 0xC3;
const PATIENCE: Duration = Duration::from_secs(120);

/// Corridor positions. 0–2 are the genesis voters, 3–6 the `Join`-mode
/// candidates, 7–9 the plain producers the eligible record omits.
const VOTERS: usize = 3;
const CANDIDATES: std::ops::Range<usize> = 3..7;
const PLAIN: std::ops::Range<usize> = 7..10;
const TOTAL: usize = 10;

/// The 4 / 3 / 3 partition, by corridor position.
const CLUSTER_A: &[usize] = &[0, 1, 2, 3];
const CLUSTER_B: &[usize] = &[4, 5, 7];
const CLUSTER_C: &[usize] = &[6, 8, 9];

/// Wait for `server` to report `Ready`, by polling.
///
/// Not `wait_ready()`: that resolves on the *next* transition into `Ready`, so
/// it is one await against a group that may pass through `NoQuorum` while a
/// joint membership converges. Polling the published code asks the same
/// question repeatedly, which is what an operator does and what a loaded runner
/// needs.
async fn becomes_ready(server: &AsterServer, what: &str) {
    let handle = server.coordination().expect("hosted");
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        match handle.lifecycle_code() {
            3 => return,
            6 => panic!("{what}: the node latched Fatal"),
            code => assert!(
                std::time::Instant::now() < deadline,
                "{what}: still lifecycle {code} after {PATIENCE:?}"
            ),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The §S4-D journal state codes this gate asserts against. Transcribed here
/// rather than imported: `JournalState` is crate-private, and a gate that read
/// the code from the same enum it is checking would prove nothing about the
/// stored value.
/// The administrator's transient eleventh identity.
const ADMIN_SECRET: [u8; 32] = [SEED ^ 0xFF; 32];

const JOURNAL_PENDING: i32 = 2;
const JOURNAL_RESOLVED: i32 = 3;

async fn within<T, F: std::future::Future<Output = T>>(what: &str, f: F) -> T {
    match tokio::time::timeout(PATIENCE, f).await {
        Ok(value) => value,
        Err(_) => panic!("{what}: no answer within {PATIENCE:?}"),
    }
}

// ── Failure artifacts ───────────────────────────────────────────────────────

/// The §S5-F artifact set: topology snapshot, policy, placement report,
/// membership timeline and client journal.
///
/// Written **only on panic**, to `target/s5f-artifacts/<test>/`. A corridor
/// failure is a sequence — which snapshot the plan was computed against, what
/// the policy said at that moment, what the group's committed membership was —
/// and a bare `assert_eq!` diff of two voter sets throws all of that away.
///
/// It owns the journal directories so that its `Drop` still finds them: a
/// `TempDir` declared beside it would be free to vanish first.
struct Artifacts {
    test: &'static str,
    log: Mutex<Vec<String>>,
    home: tempfile::TempDir,
}

impl Artifacts {
    fn new(test: &'static str) -> Arc<Self> {
        Arc::new(Self {
            test,
            log: Mutex::new(Vec::new()),
            home: tempfile::tempdir().expect("an artifact directory"),
        })
    }

    /// A journal directory this artifact set will preserve on failure.
    fn journal(&self, label: &str) -> PathBuf {
        let path = self.home.path().join(format!("journal-{label}"));
        std::fs::create_dir_all(&path).expect("a journal directory");
        path
    }

    fn record(&self, what: &str, detail: impl std::fmt::Display) {
        self.log
            .lock()
            .expect("the artifact log")
            .push(format!("── {what}\n{detail}\n"));
    }

    fn topology(&self, what: &str, snapshot: &TopologySnapshot) {
        let mut rendered = String::new();
        for (index, cluster) in snapshot.clusters().iter().enumerate() {
            rendered.push_str(&format!(
                "  cluster {index}: bridge={} members={:?}\n",
                &cluster.bridge[..8.min(cluster.bridge.len())],
                cluster
                    .members
                    .iter()
                    .map(|m| m[..8.min(m.len())].to_string())
                    .collect::<Vec<_>>()
            ));
        }
        self.record(what, rendered);
    }

    fn policy(&self, what: &str, policy: &PlacementPolicy) {
        self.record(
            what,
            format!(
                "  revision={} digest={:02x?}\n  target_voters={} target_retained_learners={}\n  \
                 minimum_separated_voter_pairs={} allow_healthy_rebalance={} grace={:?}\n  \
                 eligible={:?}\n  excluded={:?}\n  pinned={:?}",
                policy.revision,
                &policy.digest[..4],
                policy.target_voters,
                policy.target_retained_learners,
                policy.minimum_separated_voter_pairs,
                policy.allow_healthy_rebalance,
                policy.replacement_grace,
                short_set(policy.eligible.keys().copied()),
                short_set(policy.excluded.iter().copied()),
                short_set(policy.pinned_voters.iter().copied()),
            ),
        );
    }

    fn report(&self, what: &str, report: &PlacementReport) {
        self.record(
            what,
            format!(
                "  reason: {}\n  current_voters={:?}\n  selected_voters={:?}\n  \
                 grace_crossed={:?}\n  separation={:?}\n  pending={:?}",
                report.reason,
                short_set(report.current_voters.iter().copied()),
                short_set(report.selected_voters.iter().copied()),
                short_set(report.grace_crossed_members.iter().copied()),
                report.separation,
                report.pending_operation_id.map(|id| hex_of(&id)),
            ),
        );
    }
}

impl Drop for Artifacts {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/s5f-artifacts")
            .join(self.test);
        if std::fs::create_dir_all(&root).is_err() {
            return;
        }
        let timeline = self.log.lock().expect("the artifact log").join("\n");
        let _ = std::fs::write(root.join("timeline.txt"), timeline);
        copy_tree(self.home.path(), &root.join("journals"));
        eprintln!(
            "\nS5-F artifacts for `{}` written to {}\n",
            self.test,
            root.display()
        );
    }
}

fn copy_tree(from: &Path, to: &Path) {
    if std::fs::create_dir_all(to).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    for entry in entries.flatten() {
        let target = to.join(entry.file_name());
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => copy_tree(&entry.path(), &target),
            Ok(_) => {
                let _ = std::fs::copy(entry.path(), target);
            }
            Err(_) => {}
        }
    }
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn short_set(nodes: impl Iterator<Item = CoordinationNodeId>) -> Vec<String> {
    nodes.map(|n| n.to_hex()[..8].to_string()).collect()
}

// ── The corridor ────────────────────────────────────────────────────────────

/// The ten running producers plus everything needed to plan over them.
struct Corridor {
    fixture: GroupFixture,
    /// Position → node id, for all ten.
    ids: Vec<CoordinationNodeId>,
    /// The three genesis voters and the four `Join`-mode candidates, by
    /// position. `None` once shut down.
    hosts: Vec<Option<AsterServer>>,
    /// The three plain producer nodes, by position.
    plain: BTreeMap<usize, Node>,
    /// Storage directories by host position, so a shut-down voter can restart
    /// **in place** — a restart from an empty directory is a different
    /// operation, and one this corridor never performs.
    dirs: Vec<tempfile::TempDir>,
    artifacts: Arc<Artifacts>,
}

fn candidate_secret(position: usize) -> [u8; 32] {
    [SEED ^ (0x40 + position as u8); 32]
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

impl Corridor {
    /// Start all ten producers, cross-register them, and wait for the three
    /// genesis voters to be ready.
    async fn start(artifacts: Arc<Artifacts>) -> Self {
        let mut fixture = GroupFixture::new("corridor.ten", SEED, VOTERS);
        for position in CANDIDATES.chain(PLAIN) {
            fixture = fixture.admitting(candidate_secret(position));
        }

        let mut ids: Vec<CoordinationNodeId> = Vec::with_capacity(TOTAL);
        for position in 0..TOTAL {
            let raw = if position < VOTERS {
                <[u8; 32]>::try_from(fixture.node_id(position).as_slice()).expect("32 bytes")
            } else {
                node_id_of(candidate_secret(position))
            };
            ids.push(CoordinationNodeId::from_array(raw));
        }

        let mut dirs: Vec<tempfile::TempDir> = Vec::new();
        let mut hosts: Vec<Option<AsterServer>> = Vec::new();

        for position in 0..VOTERS {
            let dir = tempfile::tempdir().expect("a directory");
            let server = AsterServer::builder()
                .config(fixture.aster_config(position))
                .builtin_coordination(fixture.config(dir.path(), position))
                .start()
                .await
                .expect("a genesis voter starts");
            dirs.push(dir);
            hosts.push(Some(server));
        }

        for position in CANDIDATES {
            let secret = candidate_secret(position);
            let dir = tempfile::tempdir().expect("a directory");
            let server = AsterServer::builder()
                .config(
                    AsterConfig::builder()
                        .relay(RelayMode::Disabled)
                        .bind_addr("127.0.0.1:0")
                        .secret_key(SecretKey::from_bytes(secret))
                        .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
                        .build(),
                )
                .builtin_coordination(fixture.join_config(
                    dir.path(),
                    secret,
                    &canonical_ticket(&node_id_of(secret)),
                    &[0, 1, 2],
                ))
                .start()
                .await
                .expect("a standby candidate starts joining");
            dirs.push(dir);
            hosts.push(Some(server));
        }

        let mut plain = BTreeMap::new();
        for position in PLAIN {
            let node = Node::start(
                AsterConfig::builder()
                    .relay(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")
                    .secret_key(SecretKey::from_bytes(candidate_secret(position)))
                    .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
                    .build(),
            )
            .await
            .expect("a plain producer starts");
            plain.insert(position, node);
        }

        let corridor = Corridor {
            fixture,
            ids,
            hosts,
            plain,
            dirs,
            artifacts,
        };
        corridor.cross_register();
        assert_eq!(
            corridor.hosts.len() + corridor.plain.len(),
            TOTAL,
            "the corridor is ten producers"
        );
        for position in 0..VOTERS {
            within(
                "genesis voter readiness",
                corridor
                    .host(position)
                    .coordination()
                    .expect("hosted")
                    .wait_ready(),
            )
            .await
            .expect("the group becomes ready");
        }
        for position in CANDIDATES {
            assert_eq!(
                corridor
                    .host(position)
                    .coordination()
                    .expect("hosted")
                    .lifecycle_code(),
                2,
                "candidate {position} is Joining, not a member"
            );
        }
        corridor
    }

    fn host(&self, position: usize) -> &AsterServer {
        self.hosts[position]
            .as_ref()
            .unwrap_or_else(|| panic!("producer {position} is running"))
    }

    /// The same-host substitution for discovery every cluster gate uses:
    /// every producer learns every other producer's address, and nothing
    /// pins a port.
    fn cross_register(&self) {
        let mut addrs = Vec::new();
        for host in self.hosts.iter().flatten() {
            addrs.push(host.node().addr());
        }
        for node in self.plain.values() {
            addrs.push(node.addr());
        }
        for host in self.hosts.iter().flatten() {
            for addr in &addrs {
                if addr.node_id != host.node().addr().node_id {
                    let _ = host.node().add_node_addr(addr.clone());
                }
            }
        }
        for node in self.plain.values() {
            for addr in &addrs {
                if addr.node_id != node.addr().node_id {
                    let _ = node.add_node_addr(addr.clone());
                }
            }
        }
    }

    /// Restart genesis voters **in place**, from the directories they were
    /// shut down with, through `Existing` — the runbook's restart, not a
    /// re-bootstrap.
    async fn restart_voters(&mut self, positions: &[usize]) {
        for position in positions {
            assert!(*position < VOTERS, "only a genesis voter restarts in place");
            assert!(
                self.hosts[*position].is_none(),
                "voter {position} is already running"
            );
            let server = AsterServer::builder()
                .config(self.fixture.aster_config(*position))
                .builtin_coordination(self.fixture.existing_config(self.dirs[*position].path()))
                .start()
                .await
                .expect("the voter restarts from its own directory");
            self.hosts[*position] = Some(server);
        }
        self.cross_register();
        for position in positions {
            within(
                "restarted voter readiness",
                self.host(*position)
                    .coordination()
                    .expect("hosted")
                    .wait_ready(),
            )
            .await
            .expect("the restarted voter rejoins and becomes ready");
        }
    }

    fn group(&self) -> CoordinationGroupId {
        self.fixture.group_id()
    }

    fn position_of(&self, id: &CoordinationNodeId) -> usize {
        self.ids
            .iter()
            .position(|held| held == id)
            .unwrap_or_else(|| panic!("{} is not a corridor producer", id.to_hex()))
    }

    fn cluster_of(&self, id: &CoordinationNodeId) -> usize {
        let position = self.position_of(id);
        [CLUSTER_A, CLUSTER_B, CLUSTER_C]
            .iter()
            .position(|c| c.contains(&position))
            .expect("every producer is in a cluster")
    }

    /// An administrator client on a transient eleventh endpoint — the §8.1
    /// break-glass identity is deliberately not one of the ten.
    async fn admin(&self, journal: &Path) -> CoordinationClient {
        self.admin_seeded(journal, &[0, 1, 2]).await
    }

    /// [`admin`](Self::admin) with an explicit seed set, so a gate can point
    /// the routing walk at exactly one live member and never wait on a dial
    /// timeout to a node it has just shut down.
    async fn admin_seeded(&self, journal: &Path, seeds: &[usize]) -> CoordinationClient {
        let node = Node::start(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes(ADMIN_SECRET))
                .root_pubkey(PublicKey::from_bytes(self.fixture.authority_key()))
                .build(),
        )
        .await
        .expect("the administrator node starts");
        // Every running producer's address, not just the seeds': after a
        // placement the leader may be a member this client was never seeded
        // with, and with the relay disabled an unknown address is an
        // undialable redirect. Seeding and addressing are different things —
        // the seed set is where a routing walk *starts*.
        for host in self.hosts.iter().flatten() {
            node.add_node_addr(host.node().addr()).expect("register");
        }
        let tickets: Vec<String> = seeds
            .iter()
            .map(|i| {
                let id: [u8; 32] = self.fixture.node_id(*i).try_into().expect("32 bytes");
                canonical_ticket(&id)
            })
            .collect();
        CoordinationClient::builder(node, self.group())
            .seed_tickets(tickets)
            .journal_dir(journal)
            .build()
            .expect("the administrator client builds")
    }

    /// The committed voter set, as corridor positions.
    async fn committed_voters(&self, client: &CoordinationClient) -> Vec<usize> {
        let status = within("group_status", client.reads().group_status())
            .await
            .expect("the status cut");
        let mut positions: Vec<usize> = status
            .voters
            .iter()
            .map(|n| {
                self.position_of(&CoordinationNodeId::from_slice(&n.node_id).expect("32 bytes"))
            })
            .collect();
        positions.sort_unstable();
        positions
    }

    /// Every running host's own view of itself, for a failure that the
    /// client's status cut cannot explain: which lifecycle each is in, who it
    /// believes leads, and what membership it has committed.
    fn diagnose(&self) -> String {
        let mut rendered = String::new();
        for (position, host) in self.hosts.iter().enumerate() {
            let Some(server) = host else {
                rendered.push_str(&format!("  position {position}: shut down\n"));
                continue;
            };
            let handle = server.coordination().expect("hosted");
            let mut committed: Vec<usize> = handle
                .committed_nodes_for_testing()
                .iter()
                .filter_map(|n| self.ids.iter().position(|held| held == n))
                .collect();
            committed.sort_unstable();
            let leader = handle
                .current_leader_for_testing()
                .and_then(|id| self.ids.iter().position(|held| *held == id));
            rendered.push_str(&format!(
                "  position {position}: lifecycle={} leader={leader:?} committed={committed:?} \
                 joint={} last_applied={:?}\n",
                handle.lifecycle_code(),
                handle.membership_is_joint_for_testing(),
                handle.last_applied_for_testing(),
            ));
        }
        rendered
    }

    /// Wait for the committed uniform membership to be exactly `expected`
    /// (positions), with no learner residue and no pending operation.
    async fn settles_on(&self, client: &CoordinationClient, expected: &[usize]) {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            // A membership transition legitimately passes through moments with
            // no leader, and a status read taken in one of them is not a
            // failure to settle. Only the deadline is.
            let status = match within("group_status", client.reads().group_status()).await {
                Ok(status) => status,
                Err(err) => {
                    self.artifacts
                        .record("membership cut", format!("  unreadable: {err}"));
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the membership never settled on {expected:?}; the last status \
                         read failed: {err}\n{}",
                        self.diagnose()
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
            };
            let mut positions: Vec<usize> = status
                .voters
                .iter()
                .map(|n| {
                    self.position_of(&CoordinationNodeId::from_slice(&n.node_id).expect("32 bytes"))
                })
                .collect();
            positions.sort_unstable();
            self.artifacts.record(
                "membership cut",
                format!(
                    "  voters={positions:?} learners={} joint={} pending={:?}",
                    status.learners.len(),
                    status.membership_joint,
                    status.pending_operation_id.as_ref().map(|id| hex_of(id))
                ),
            );
            if positions == expected
                && status.learners.is_empty()
                && !status.membership_joint
                && status.pending_operation_id.is_none()
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the membership never settled on {expected:?}; last cut was {positions:?}\n{}",
                self.diagnose()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn shutdown(mut self) {
        for host in self.hosts.iter_mut() {
            if let Some(server) = host.take() {
                server.shutdown().await;
            }
        }
    }
}

// ── Controlled topology evidence ────────────────────────────────────────────

/// One deterministic snapshot over corridor positions.
///
/// `clusters` are positions; every cross-cluster pair present in `live` is
/// corroborated as far, so with coverage 1 each distinct pair of clusters is
/// `Separated` and no pair is `Unknown`. `bridge_pick` chooses which member a
/// cluster names as its bridge — the lever the "bridge rank is not a voter
/// rank" assertion pulls.
fn evidence(
    ids: &[CoordinationNodeId],
    clusters: &[&[usize]],
    live: &BTreeSet<usize>,
    bridge_pick: impl Fn(usize, &[String]) -> String,
    corroborate: bool,
) -> TopologySnapshot {
    let present: Vec<Vec<usize>> = clusters
        .iter()
        .map(|members| {
            members
                .iter()
                .copied()
                .filter(|p| live.contains(p))
                .collect()
        })
        .filter(|members: &Vec<usize>| !members.is_empty())
        .collect();

    let views: Vec<CoreClusterView> = present
        .iter()
        .enumerate()
        .map(|(index, members)| {
            let mut hexes: Vec<String> = members.iter().map(|p| ids[*p].to_hex()).collect();
            hexes.sort();
            CoreClusterView {
                bridge: bridge_pick(index, &hexes),
                witnesses: Vec::new(),
                members: hexes,
            }
        })
        .collect();

    let mut far: Vec<(String, String)> = Vec::new();
    if corroborate {
        for (i, left) in present.iter().enumerate() {
            for right in &present[i + 1..] {
                for a in left {
                    for b in right {
                        far.push((ids[*a].to_hex(), ids[*b].to_hex()));
                    }
                }
            }
        }
    }
    TopologySnapshot::from_view(Arc::new(CoreTopoSharedView::from_parts(views, far, 1)))
}

/// The bridge every cluster would elect by the fixture's default rule.
fn lowest_bridge(_index: usize, members: &[String]) -> String {
    members.first().cloned().unwrap_or_default()
}

/// The opposite choice, so "same evidence, different bridge" is expressible.
fn highest_bridge(_index: usize, members: &[String]) -> String {
    members.last().cloned().unwrap_or_default()
}

/// The corridor at full health: all ten live, 4 / 3 / 3, fully corroborated.
fn healthy(ids: &[CoordinationNodeId]) -> TopologySnapshot {
    evidence(
        ids,
        &[CLUSTER_A, CLUSTER_B, CLUSTER_C],
        &(0..TOTAL).collect::<BTreeSet<usize>>(),
        lowest_bridge,
        true,
    )
}

// ── Policy: the explicit eligible-host record ───────────────────────────────

/// The seven coordination-capable producers, in corridor order. `p7`–`p9` are
/// admitted and live and deliberately absent: eligibility is the
/// application's record, never admission and never presence.
const ELIGIBLE: &[usize] = &[0, 1, 2, 3, 4, 5, 6];

fn seed_for(corridor: &Corridor, position: usize) -> MemberSeedV1 {
    let secret = candidate_secret(position);
    let id = node_id_of(secret);
    let ticket = canonical_ticket(&id);
    MemberSeedV1 {
        node_id: id.to_vec(),
        ticket: ticket.clone(),
        signed_join_permit: Some(corridor.fixture.join_permit_with_ticket(
            secret,
            &ticket,
            &[0, 1, 2],
        )),
    }
}

/// The corridor's policy record.
///
/// `unseeded` names positions whose candidate material the record deliberately
/// omits, so the planner answers `NeedsProvisioning` and the application's
/// provisioning callback is exercised for real rather than assumed.
fn corridor_policy(
    corridor: &Corridor,
    revision: u64,
    rebalance: bool,
    grace: Duration,
    unseeded: &[usize],
) -> PlacementPolicy {
    let mut eligible: BTreeMap<CoordinationNodeId, PlacementCandidate> = BTreeMap::new();
    for position in ELIGIBLE {
        let id = corridor.ids[*position];
        let member_seed = if unseeded.contains(position) || *position < VOTERS {
            // A sitting genesis voter needs no seed: it is already a member.
            None
        } else {
            Some(seed_for(corridor, *position))
        };
        eligible.insert(
            id,
            PlacementCandidate {
                node_id: id,
                member_seed,
            },
        );
    }
    let mut digest = [0u8; 32];
    digest[..8].copy_from_slice(&revision.to_be_bytes());
    PlacementPolicy {
        revision,
        digest,
        authority_public_key: corridor.fixture.authority_key(),
        authority_domain: corridor.fixture.domain().to_string(),
        target_voters: 3,
        target_retained_learners: 0,
        eligible,
        pinned_voters: BTreeSet::new(),
        excluded: BTreeSet::new(),
        // The corridor's frozen requirement: separation as a requirement, not
        // a preference, and all three pairs of it.
        minimum_separated_voter_pairs: 3,
        replacement_grace: grace,
        allow_healthy_rebalance: rebalance,
    }
}

// ── Manager wiring ──────────────────────────────────────────────────────────

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

/// The application's provisioning callback, recording every idempotency tuple
/// it was asked for so the corridor can assert *which* candidate was
/// provisioned and how often.
/// One recorded provisioning call: which candidate, under which policy digest.
type ProvisionCall = (CoordinationNodeId, [u8; 32]);

#[derive(Clone)]
struct ProvisioningLog(Arc<Mutex<Vec<ProvisionCall>>>);

impl ProvisioningLog {
    fn new() -> Self {
        ProvisioningLog(Arc::new(Mutex::new(Vec::new())))
    }
    fn calls(&self) -> Vec<ProvisionCall> {
        self.0.lock().expect("the provisioning log").clone()
    }
}

struct CorridorProvisioner {
    seeds: BTreeMap<CoordinationNodeId, MemberSeedV1>,
    calls: ProvisioningLog,
}

#[async_trait::async_trait]
impl PlacementProvisioner for CorridorProvisioner {
    async fn provision(&self, request: &ProvisionRequest) -> aster::Result<MemberSeedV1> {
        self.calls
            .0
            .lock()
            .expect("the provisioning log")
            .push((request.node_id, request.policy_digest));
        Ok(self
            .seeds
            .get(&request.node_id)
            .unwrap_or_else(|| panic!("no seed staged for {}", request.node_id.to_hex()))
            .clone())
    }
}

/// A provisioner that must never be called.
struct NoProvisioning;

#[async_trait::async_trait]
impl PlacementProvisioner for NoProvisioning {
    async fn provision(&self, request: &ProvisionRequest) -> aster::Result<MemberSeedV1> {
        panic!("provisioning was not expected for {request:?}");
    }
}

/// Build a `PlacementInput` from the corridor's **real** linearizable status
/// cut, so the pure assertions below are made against the group's own
/// committed membership rather than a hand-built one.
async fn observed_input(
    corridor: &Corridor,
    client: &CoordinationClient,
    policy: PlacementPolicy,
    topology: TopologySnapshot,
) -> PlacementInput {
    let status = within("group_status", client.reads().group_status())
        .await
        .expect("the status cut");
    let ids = |nodes: &[aster::coordination::contract::NodeIdV1]| -> BTreeSet<CoordinationNodeId> {
        nodes
            .iter()
            .map(|n| CoordinationNodeId::from_slice(&n.node_id).expect("32 bytes"))
            .collect()
    };
    PlacementInput {
        group_id: corridor.group(),
        genesis_hash: <[u8; 32]>::try_from(status.genesis_hash.as_slice()).expect("32 bytes"),
        deployment_profile_code: status.deployment_profile_code,
        membership_log_id: status.membership_log_id.clone(),
        current_voters: ids(&status.voters),
        current_learners: ids(&status.learners),
        active_membership_intent: status.pending_operation_id.is_some(),
        grace_crossed_members: BTreeSet::new(),
        policy,
        topology,
    }
}

// ── The corridor gate ───────────────────────────────────────────────────────

/// **The corridor, end to end.**
///
/// One group, five frozen assertions in the order an operator would meet them:
///
/// 1. initial placement selects one voter per cluster on positive separation,
///    and the same selection results when only the bridge changes;
/// 2. RTT/cluster jitter and a bridge change move no healthy voter;
/// 3. `Unknown` evidence never claims proved diversity;
/// 4. an admitted, live, but ineligible producer is never selected;
/// 5. one voter dies, the grace expires, exactly one selected replacement is
///    provisioned and caught up, and the group returns to exactly three voters
///    with the other two unmoved.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_corridor_places_one_voter_per_cluster_and_repairs_one_loss() {
    let artifacts = Artifacts::new("corridor-place-and-repair");
    let mut corridor = Corridor::start(artifacts.clone()).await;
    let admin_journal = artifacts.journal("admin");
    let admin = corridor.admin(&admin_journal).await;

    // The corridor starts undiverse on purpose: all three voters in cluster A.
    let start = corridor.committed_voters(&admin).await;
    assert_eq!(
        start,
        vec![0, 1, 2],
        "the genesis voters are the first three"
    );
    let start_clusters: BTreeSet<usize> = start
        .iter()
        .map(|p| corridor.cluster_of(&corridor.ids[*p]))
        .collect();
    assert_eq!(
        start_clusters.len(),
        1,
        "the corridor starts with every voter in one cluster"
    );

    let healthy_view = healthy(&corridor.ids);
    artifacts.topology("initial evidence", &healthy_view);

    // ── 1. Initial placement, and the bridge is not a voter rank ────────────
    let spread_policy = corridor_policy(&corridor, 1, true, Duration::from_secs(600), &[]);
    artifacts.policy("initial policy", &spread_policy);

    let planned = plan(
        &observed_input(
            &corridor,
            &admin,
            spread_policy.clone(),
            healthy_view.clone(),
        )
        .await,
    );
    let selection = match &planned {
        PlacementRecommendation::Plan(chosen) => (**chosen).clone(),
        other => panic!("the corridor's initial placement found no plan: {other:?}"),
    };
    let selected_clusters: BTreeSet<usize> = selection
        .desired_voters
        .iter()
        .map(|v| corridor.cluster_of(v))
        .collect();
    assert_eq!(
        selected_clusters.len(),
        3,
        "one voter per cluster: {:?}",
        short_set(selection.desired_voters.iter().copied())
    );
    assert_eq!(
        selection.desired_voters.len(),
        3,
        "exactly three, never four"
    );
    assert!(
        selection.separation.diversity_proved,
        "the selection reports positive separation: {:?}",
        selection.separation
    );
    assert_eq!(
        selection.separation.separated_pairs, 3,
        "all three pairs are positively separated: {:?}",
        selection.separation
    );
    assert_eq!(selection.separation.unknown_pairs, 0);

    // The same evidence with every cluster naming its *other* bridge selects
    // the same voters. A planner that ranked by bridge would move one.
    let other_bridges = evidence(
        &corridor.ids,
        &[CLUSTER_A, CLUSTER_B, CLUSTER_C],
        &(0..TOTAL).collect::<BTreeSet<usize>>(),
        highest_bridge,
        true,
    );
    artifacts.topology("same evidence, other bridges", &other_bridges);
    match plan(&observed_input(&corridor, &admin, spread_policy.clone(), other_bridges).await) {
        PlacementRecommendation::Plan(other) => assert_eq!(
            other.desired_voters, selection.desired_voters,
            "bridge rank moved a voter"
        ),
        other => panic!("the bridge-permuted corridor found no plan: {other:?}"),
    }

    // ── 3. Unknown evidence never claims proved diversity ───────────────────
    // The same clusters with no corroborated far pair at all: cardinality is
    // reachable, diversity is not.
    let uncorroborated = evidence(
        &corridor.ids,
        &[CLUSTER_A, CLUSTER_B, CLUSTER_C],
        &(0..TOTAL).collect::<BTreeSet<usize>>(),
        lowest_bridge,
        false,
    );
    artifacts.topology("uncorroborated evidence", &uncorroborated);
    match plan(&observed_input(&corridor, &admin, spread_policy.clone(), uncorroborated).await) {
        PlacementRecommendation::InsufficientEvidence { missing } => {
            assert!(
                missing.contains("separated pairs"),
                "the refusal names the unproved separation: {missing}"
            );
            artifacts.record("unknown evidence", &missing);
        }
        other => panic!("Unknown evidence was counted as separation: {other:?}"),
    }

    // ── 4. An admitted, live, ineligible producer is never selected ─────────
    for position in PLAIN {
        assert!(
            corridor.plain.contains_key(&position),
            "producer {position} is a live admitted node"
        );
        assert!(
            !selection.desired_voters.contains(&corridor.ids[position]),
            "producer {position} is admitted and live but not in the eligible record"
        );
    }

    // Absence from a selection that had eligible candidates to spare proves
    // little. **Starve the record**: remove every eligible cluster-B candidate
    // so the only live node that could restore three separated pairs is
    // producer 7 — admitted, live, in cluster B, and not in the eligible
    // record. The planner must refuse rather than reach for it, and must not
    // *name* it in any answer: `NeedsProvisioning` names a candidate as surely
    // as `Plan` does.
    let starved = {
        let mut policy = corridor_policy(&corridor, 9, true, Duration::from_secs(600), &[]);
        for position in CLUSTER_B {
            policy.eligible.remove(&corridor.ids[*position]);
        }
        policy
    };
    artifacts.policy("starved of eligible cluster-B candidates", &starved);
    let starved_answer =
        plan(&observed_input(&corridor, &admin, starved, healthy_view.clone()).await);
    artifacts.record(
        "starved of eligible cluster-B candidates",
        format!("  {starved_answer:?}"),
    );
    assert!(
        !matches!(starved_answer, PlacementRecommendation::Plan(_)),
        "with no eligible cluster-B candidate the planner must refuse, not \
         reach for an ineligible one: {starved_answer:?}"
    );
    let rendered = format!("{starved_answer:?}");
    for position in PLAIN {
        assert!(
            !rendered.contains(&corridor.ids[position].to_hex()),
            "producer {position} is admitted and live but not eligible; no \
             recommendation may name it: {rendered}"
        );
    }

    // ── Submit the initial placement for real ───────────────────────────────
    let mut manager = CoordinationPlacementManager::new(
        admin.clone(),
        ScriptedTopology(Mutex::new(vec![healthy_view.clone()])),
        ScriptedPolicy(Mutex::new(vec![spread_policy.clone()])),
        NoProvisioning,
    );
    let report = within("initial reconcile", manager.reconcile())
        .await
        .expect("a report");
    artifacts.report("initial placement", &report);
    assert!(
        report.pending_operation_id.is_some(),
        "the initial placement was submitted: {}",
        report.reason
    );
    assert_eq!(
        report.selected_voters, selection.desired_voters,
        "the submitted plan is the one the corridor planned"
    );

    let mut expected: Vec<usize> = selection
        .desired_voters
        .iter()
        .map(|v| corridor.position_of(v))
        .collect();
    expected.sort_unstable();
    // Settle first: the committed uniform membership is what proves the two new
    // voters were caught up and promoted. Readiness is the follow-up check, and
    // is polled rather than awaited once.
    corridor.settles_on(&admin, &expected).await;
    for position in &expected {
        if *position >= VOTERS {
            becomes_ready(corridor.host(*position), "a newly placed voter").await;
        }
    }
    let spread_clusters: BTreeSet<usize> = expected
        .iter()
        .map(|p| corridor.cluster_of(&corridor.ids[*p]))
        .collect();
    assert_eq!(
        spread_clusters.len(),
        3,
        "the committed membership is one voter per cluster"
    );

    // ── 2. Jitter and a bridge change move no healthy voter ─────────────────
    // Rebalance is now off, which is what an operator runs steady-state.
    // Cluster composition shuffles among non-voters and the bridges flip; the
    // voters stay live and healthy throughout.
    let steady = corridor_policy(&corridor, 2, false, Duration::from_secs(600), &[]);
    artifacts.policy("steady-state policy", &steady);
    let jittered = |bridge: fn(usize, &[String]) -> String, moved: usize| {
        // One plain producer drifts from cluster B to cluster C, which is
        // exactly what RTT jitter looks like once it is derived into
        // clusters. No voter changes cluster.
        let mut b: Vec<usize> = CLUSTER_B.to_vec();
        let mut c: Vec<usize> = CLUSTER_C.to_vec();
        if let Some(index) = b.iter().position(|p| *p == moved) {
            b.remove(index);
            c.push(moved);
            c.sort_unstable();
        }
        evidence(
            &corridor.ids,
            &[CLUSTER_A, b.as_slice(), c.as_slice()],
            &(0..TOTAL).collect::<BTreeSet<usize>>(),
            bridge,
            true,
        )
    };
    for (label, view) in [
        ("jitter: p7 drifts B→C", jittered(lowest_bridge, 7)),
        ("jitter + bridge flip", jittered(highest_bridge, 7)),
        ("bridge flip only", other_bridges_of(&corridor)),
    ] {
        artifacts.topology(label, &view);
        let mut steady_manager = CoordinationPlacementManager::new(
            admin.clone(),
            ScriptedTopology(Mutex::new(vec![view.clone()])),
            ScriptedPolicy(Mutex::new(vec![steady.clone()])),
            NoProvisioning,
        );
        let report = within("steady reconcile", steady_manager.reconcile())
            .await
            .expect("a report");
        artifacts.report(label, &report);
        assert!(
            report.pending_operation_id.is_none(),
            "{label} moved a healthy voter: {}",
            report.reason
        );
        assert_eq!(
            corridor.committed_voters(&admin).await,
            expected,
            "{label} changed the committed voter set"
        );
    }

    // ── 5. One voter dies; grace expires; exactly one replacement ───────────
    let victim = *expected
        .iter()
        .find(|p| corridor.cluster_of(&corridor.ids[**p]) == 1)
        .expect("cluster B holds one voter");
    let survivors: Vec<usize> = expected.iter().copied().filter(|p| *p != victim).collect();
    // The only other eligible producer in cluster B, and the corridor's only
    // way back to three separated pairs.
    let replacement = *CLUSTER_B
        .iter()
        .find(|p| ELIGIBLE.contains(p) && **p != victim)
        .expect("cluster B holds a second eligible producer");
    artifacts.record(
        "voter loss",
        format!("  victim=position {victim}  replacement=position {replacement}  survivors={survivors:?}"),
    );

    corridor.hosts[victim]
        .take()
        .expect("the victim is running")
        .shutdown()
        .await;

    let live_without_victim: BTreeSet<usize> = (0..TOTAL).filter(|p| *p != victim).collect();
    let bereaved = evidence(
        &corridor.ids,
        &[CLUSTER_A, CLUSTER_B, CLUSTER_C],
        &live_without_victim,
        lowest_bridge,
        true,
    );
    artifacts.topology("evidence after the loss", &bereaved);

    let grace = Duration::from_millis(500);
    // The replacement's seed is deliberately absent from the record, so the
    // application's provisioning callback runs for real.
    let repair_policy = corridor_policy(&corridor, 3, false, grace, &[replacement]);
    artifacts.policy("repair policy", &repair_policy);
    let provisioning = ProvisioningLog::new();
    let provisioner = CorridorProvisioner {
        seeds: [(corridor.ids[replacement], seed_for(&corridor, replacement))]
            .into_iter()
            .collect(),
        calls: provisioning.clone(),
    };
    let mut repair = CoordinationPlacementManager::new(
        admin.clone(),
        ScriptedTopology(Mutex::new(vec![bereaved.clone()])),
        ScriptedPolicy(Mutex::new(vec![repair_policy.clone()])),
        provisioner,
    );

    // The first pass notes the absence and submits nothing: the grace has not
    // been observed to pass, and an unobserved absence is not a failure.
    let first = within("first repair reconcile", repair.reconcile())
        .await
        .expect("a report");
    artifacts.report("repair: before grace", &first);
    assert!(
        first.pending_operation_id.is_none(),
        "no command before the grace: {}",
        first.reason
    );
    assert_eq!(
        corridor.committed_voters(&admin).await,
        expected,
        "nothing moved before the grace"
    );

    tokio::time::sleep(grace + Duration::from_millis(400)).await;

    let repaired = within("repair reconcile", repair.reconcile())
        .await
        .expect("a report");
    artifacts.report("repair: after grace", &repaired);
    assert!(
        repaired.pending_operation_id.is_some(),
        "the crossed grace submitted a replacement: {}",
        repaired.reason
    );
    assert_eq!(
        repaired.selected_voters.len(),
        3,
        "exactly three voters were selected"
    );
    assert!(
        repaired
            .selected_voters
            .contains(&corridor.ids[replacement]),
        "the selection names the one eligible cluster-B replacement"
    );
    for survivor in &survivors {
        assert!(
            repaired.selected_voters.contains(&corridor.ids[*survivor]),
            "healthy voter {survivor} stayed put"
        );
    }
    assert!(
        !repaired.selected_voters.contains(&corridor.ids[victim]),
        "the lost voter left the selection"
    );

    // The provisioning callback ran, exactly once, for exactly the selected
    // candidate, keyed by the policy digest the plan was computed against.
    let calls = provisioning.calls();
    assert_eq!(
        calls.len(),
        1,
        "one candidate was provisioned, not a set: {:?}",
        short_set(calls.iter().map(|(node, _)| *node))
    );
    assert_eq!(calls[0].0, corridor.ids[replacement]);
    assert_eq!(
        calls[0].1, repair_policy.digest,
        "the idempotency tuple names the policy the plan was computed under"
    );

    let mut settled: Vec<usize> = survivors.clone();
    settled.push(replacement);
    settled.sort_unstable();
    corridor.settles_on(&admin, &settled).await;
    becomes_ready(corridor.host(replacement), "the replacement").await;

    let final_clusters: BTreeSet<usize> = settled
        .iter()
        .map(|p| corridor.cluster_of(&corridor.ids[*p]))
        .collect();
    assert_eq!(
        final_clusters.len(),
        3,
        "the repaired membership is still one voter per cluster"
    );

    corridor.shutdown().await;
}

/// The corridor's healthy evidence with every bridge flipped — extracted so
/// the jitter loop can name it beside the two drifting views.
fn other_bridges_of(corridor: &Corridor) -> TopologySnapshot {
    evidence(
        &corridor.ids,
        &[CLUSTER_A, CLUSTER_B, CLUSTER_C],
        &(0..TOTAL).collect::<BTreeSet<usize>>(),
        highest_bridge,
        true,
    )
}

/// **Two racing corridor managers commit exactly one intent, and the group
/// commits exactly one transition.**
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_racing_corridor_managers_commit_one_intent_and_one_transition() {
    let artifacts = Artifacts::new("corridor-racing-managers");
    let corridor = Corridor::start(artifacts.clone()).await;
    let observer = corridor.admin(&artifacts.journal("observer")).await;
    let before = corridor.committed_voters(&observer).await;

    let policy = corridor_policy(&corridor, 1, true, Duration::from_secs(600), &[]);
    artifacts.policy("racing policy", &policy);
    let view = healthy(&corridor.ids);
    artifacts.topology("racing evidence", &view);

    let client_a = corridor.admin(&artifacts.journal("manager-a")).await;
    let client_b = corridor.admin(&artifacts.journal("manager-b")).await;
    let manager_of = |client: CoordinationClient| {
        CoordinationPlacementManager::new(
            client,
            ScriptedTopology(Mutex::new(vec![view.clone()])),
            ScriptedPolicy(Mutex::new(vec![policy.clone()])),
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
    artifacts.report("manager A", &report_a);
    artifacts.report("manager B", &report_b);

    let accepted: Vec<&PlacementReport> = [&report_a, &report_b]
        .into_iter()
        .filter(|r| r.pending_operation_id.is_some())
        .collect();
    assert_eq!(
        accepted.len(),
        1,
        "exactly one active intent:\n  A: {}\n  B: {}",
        report_a.reason,
        report_b.reason
    );

    let status = within("status", observer.reads().group_status())
        .await
        .expect("the status cut");
    assert_eq!(
        status.pending_operation_id.as_deref(),
        accepted[0]
            .pending_operation_id
            .as_ref()
            .map(|id| id.as_slice()),
        "the group's one committed intent is the winner's"
    );

    // And exactly one transition: the settled membership is the winner's
    // selection, reached once.
    let mut expected: Vec<usize> = accepted[0]
        .selected_voters
        .iter()
        .map(|v| corridor.position_of(v))
        .collect();
    expected.sort_unstable();
    corridor.settles_on(&observer, &expected).await;
    for position in &expected {
        if *position >= VOTERS {
            becomes_ready(corridor.host(*position), "a placed voter").await;
        }
    }
    assert_ne!(expected, before, "the one transition actually moved");

    corridor.shutdown().await;
}

/// **A manager killed before the grace restarts the timer.** The absence is
/// real and continuous, but the timers are manager-local safety state: a fresh
/// manager starts with none, which delays replacement and never shortens it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_corridor_manager_killed_before_the_grace_restarts_the_timer() {
    let artifacts = Artifacts::new("corridor-grace-restart");
    let mut corridor = Corridor::start(artifacts.clone()).await;
    let admin = corridor.admin(&artifacts.journal("admin")).await;
    let before = corridor.committed_voters(&admin).await;

    // Voter 2 dies; the remaining two still hold quorum.
    corridor.hosts[2]
        .take()
        .expect("voter 2 is running")
        .shutdown()
        .await;
    let live: BTreeSet<usize> = (0..TOTAL).filter(|p| *p != 2).collect();
    let view = evidence(
        &corridor.ids,
        &[CLUSTER_A, CLUSTER_B, CLUSTER_C],
        &live,
        lowest_bridge,
        true,
    );
    artifacts.topology("evidence with voter 2 absent", &view);

    let grace = Duration::from_millis(600);
    // Separation is a preference here, not a requirement: the point of this
    // gate is the timer, and demanding three separated pairs from an
    // all-in-one-cluster membership would refuse for a different reason.
    let mut policy = corridor_policy(&corridor, 1, false, grace, &[3]);
    policy.minimum_separated_voter_pairs = 0;
    artifacts.policy("grace policy", &policy);
    let seeds: BTreeMap<CoordinationNodeId, MemberSeedV1> =
        [(corridor.ids[3], seed_for(&corridor, 3))]
            .into_iter()
            .collect();
    let provisioner = || CorridorProvisioner {
        seeds: seeds.clone(),
        calls: ProvisioningLog::new(),
    };

    let mut long_lived = CoordinationPlacementManager::new(
        admin.clone(),
        ScriptedTopology(Mutex::new(vec![view.clone()])),
        ScriptedPolicy(Mutex::new(vec![policy.clone()])),
        provisioner(),
    );
    let first = within("first reconcile", long_lived.reconcile())
        .await
        .expect("a report");
    artifacts.report("long-lived: first pass", &first);
    assert!(
        first.pending_operation_id.is_none(),
        "no command before the grace: {}",
        first.reason
    );

    tokio::time::sleep(grace + Duration::from_millis(400)).await;

    // The manager dies here. Its replacement starts with no timers at all.
    let restarted_client = corridor.admin(&artifacts.journal("restarted")).await;
    let mut restarted = CoordinationPlacementManager::new(
        restarted_client,
        ScriptedTopology(Mutex::new(vec![view.clone()])),
        ScriptedPolicy(Mutex::new(vec![policy.clone()])),
        provisioner(),
    );
    let fresh = within("restarted reconcile", restarted.reconcile())
        .await
        .expect("a report");
    artifacts.report("restarted manager", &fresh);
    assert!(
        fresh.pending_operation_id.is_none(),
        "the restarted manager restarted the grace rather than inheriting it: {}",
        fresh.reason
    );
    assert!(
        fresh.grace_crossed_members.is_empty(),
        "a fresh manager has crossed nothing yet: {:?}",
        short_set(fresh.grace_crossed_members.iter().copied())
    );
    assert_eq!(
        corridor.committed_voters(&admin).await,
        before,
        "no membership moved while the grace restarted"
    );

    // The manager that held its timer across the whole grace does replace —
    // so the assertion above is about the restart, not about a grace that
    // never elapses.
    let held = within("held-timer reconcile", long_lived.reconcile())
        .await
        .expect("a report");
    artifacts.report("long-lived: after grace", &held);
    assert!(
        held.pending_operation_id.is_some(),
        "the held timer crossed the grace and submitted: {}",
        held.reason
    );

    corridor.shutdown().await;
}

/// **A manager killed after the journal fsync retries the exact operation id
/// and request bytes.**
///
/// Two halves, because the journal has two post-fsync states and the retry
/// contract covers both.
///
/// **A — a `Pending` entry is re-sent, byte for byte.** An ordinary mutation
/// fsyncs its exact request frame before any send; with the quorum gone the
/// send cannot resolve, and the entry stays open. The manager dies. Quorum
/// returns, the next life opens the same journal, and the record still holds
/// the identical bytes, action hash and sequence — which is what `resume`
/// re-sends. It re-sends *those*, not a freshly encoded equivalent.
///
/// **B — a resolved membership request redelivers the same operation id.** A
/// membership change is deliberately *unable* to reach `Pending` without a
/// quorum: it validates against a linearizable status cut first and refuses to
/// journal at all when that cut is unreachable, so nothing is at risk and the
/// caller simply retries. Its post-fsync death is therefore the
/// `ResolvedAwaitingAck` one — the reply fsynced, the acknowledgement never
/// made. The next life redelivers that exact saved reply, and the group holds
/// the intent for the operation id the dead manager minted rather than one
/// from a new plan. `resolve` carries the request frame and its hash forward
/// unchanged, so the same byte-level comparison applies here too.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_corridor_manager_killed_after_the_journal_fsync_retries_the_same_request() {
    let artifacts = Artifacts::new("corridor-journal-retry");
    let mut corridor = Corridor::start(artifacts.clone()).await;

    // ── A. A `Pending` entry is re-sent byte for byte ───────────────────────
    let pending_journal = artifacts.journal("pending");
    let resource = aster::coordination::contract::ResourceKeyV1 {
        namespace: "corridor.ten".into(),
        key: b"journalled".to_vec(),
    };
    // The administrator is not a grantee: §8 rights are committed, and an
    // `acquire` without them is refused before it is ever journalled. Grant
    // them while the group still has a quorum.
    {
        let granting = corridor.admin(&artifacts.journal("granting")).await;
        let granted = within(
            "grant",
            granting.set_grant(
                resource.clone(),
                node_id_of(ADMIN_SECRET),
                RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
                0,
            ),
        )
        .await
        .expect("ROOT grants");
        assert_eq!(granted.reply().outcome_code, 6, "GrantSet");
        within("ack", granted.acknowledge()).await.expect("ack");
    }

    let (pending_bytes, pending_hash, pending_sequence);
    {
        // Seeded with voter 0 alone, so every routing walk speaks to one
        // live, promptly-refusing member and the timing never depends on
        // dial timeouts to dead nodes.
        let client = corridor.admin_seeded(&pending_journal, &[0]).await;

        corridor.hosts[1]
            .take()
            .expect("voter 1 is running")
            .shutdown()
            .await;
        corridor.hosts[2]
            .take()
            .expect("voter 2 is running")
            .shutdown()
            .await;

        // The manager dies *while the request is outstanding*. Whether the
        // client had already given up or was still walking candidates is not
        // this gate's subject and is not something a killed process gets to
        // choose: either way the entry is fsynced and unresolved, and the
        // future is dropped where a `SIGKILL` would have landed.
        let doomed = tokio::time::timeout(
            Duration::from_secs(15),
            client.acquire(resource.clone(), 60_000),
        )
        .await;
        artifacts.record(
            "doomed submission",
            match &doomed {
                Ok(Ok(_)) => "resolved — the gate's premise is broken".to_string(),
                Ok(Err(err)) => format!("  refused before the death: {err}"),
                Err(_) => "  still outstanding at the death".to_string(),
            },
        );
        assert!(
            !matches!(doomed, Ok(Ok(_))),
            "no quorum may resolve the request"
        );

        let (state, method, bytes, hash, sequence) = client.journal_entry_for_testing().await;
        assert_eq!(
            state, JOURNAL_PENDING,
            "the entry is Pending: the request fsynced, nothing resolved"
        );
        assert_eq!(method, "acquire");
        assert!(!bytes.is_empty(), "the exact request frame is durable");
        artifacts.record(
            "journalled Pending entry",
            format!(
                "  method={method} sequence={sequence} bytes={} hash={}",
                bytes.len(),
                hex_of(&hash)
            ),
        );
        (pending_bytes, pending_hash, pending_sequence) = (bytes, hash, sequence);
        // The manager process dies here, with the entry pending.
    }

    corridor.restart_voters(&[1, 2]).await;

    let resumed = corridor.admin_seeded(&pending_journal, &[0, 1, 2]).await;
    let (state, method, bytes, hash, sequence) = resumed.journal_entry_for_testing().await;
    assert_eq!(
        state, JOURNAL_PENDING,
        "the next life finds the same Pending entry"
    );
    assert_eq!(method, "acquire");
    assert_eq!(
        bytes, pending_bytes,
        "the retry carries the exact request bytes, byte for byte"
    );
    assert_eq!(hash, pending_hash, "and the exact action hash");
    assert_eq!(sequence, pending_sequence, "and the same sequence");

    let delivered = within("resume", resumed.resume())
        .await
        .expect("the resume drives the journalled request")
        .expect("a redelivered result");
    artifacts.record(
        "resumed Pending entry",
        format!(
            "  outcome={} sequence={}",
            delivered.reply().outcome_code,
            delivered.reply().sequence
        ),
    );
    assert_eq!(
        delivered.reply().sequence,
        pending_sequence,
        "the resolved entry is the journalled one, not a new sequence"
    );
    within("ack", delivered.acknowledge()).await.expect("ack");

    // ── B. A resolved membership request redelivers the same operation id ───
    let membership_journal = artifacts.journal("membership");
    let operation_id: [u8; 16] = *b"corridor-retry01";
    let desired_positions = [0usize, 1, 3];
    let (membership_bytes, membership_hash, membership_sequence, membership_outcome);
    {
        let client = corridor.admin(&membership_journal).await;
        let status = within("status", client.reads().group_status())
            .await
            .expect("the status cut");
        let mut desired: Vec<aster::coordination::contract::NodeIdV1> = desired_positions
            .iter()
            .map(|p| aster::coordination::contract::NodeIdV1 {
                node_id: corridor.ids[*p].as_bytes().to_vec(),
            })
            .collect();
        desired.sort_by(|l, r| l.node_id.cmp(&r.node_id));

        let resolved = within(
            "membership submission",
            client.change_membership(
                operation_id,
                status.membership_log_id.clone(),
                desired,
                Vec::new(),
                vec![seed_for(&corridor, 3)],
            ),
        )
        .await
        .expect("the plan resolves");
        membership_outcome = resolved.reply().outcome_code;
        assert_eq!(membership_outcome, 8, "MembershipAccepted");
        // The reply is fsynced. The manager dies **before** acknowledging.
        drop(resolved);

        let (state, method, bytes, hash, sequence) = client.journal_entry_for_testing().await;
        assert_eq!(state, JOURNAL_RESOLVED, "the entry is ResolvedAwaitingAck");
        assert_eq!(method, "change_membership");
        artifacts.record(
            "journalled membership entry",
            format!(
                "  method={method} sequence={sequence} bytes={} hash={}",
                bytes.len(),
                hex_of(&hash)
            ),
        );
        (membership_bytes, membership_hash, membership_sequence) = (bytes, hash, sequence);
    }

    let next_life = corridor.admin(&membership_journal).await;
    let (state, method, bytes, hash, sequence) = next_life.journal_entry_for_testing().await;
    assert_eq!(
        state, JOURNAL_RESOLVED,
        "the saved result survived the death"
    );
    assert_eq!(method, "change_membership");
    assert_eq!(
        bytes, membership_bytes,
        "the record still names the exact request the reply answers"
    );
    assert_eq!(hash, membership_hash);
    assert_eq!(sequence, membership_sequence);

    let view = healthy(&corridor.ids);
    let mut policy = corridor_policy(&corridor, 1, true, Duration::from_secs(600), &[]);
    // Separation is not what this gate is about, and the journalled plan
    // deliberately does not satisfy it.
    policy.minimum_separated_voter_pairs = 0;
    artifacts.policy("retry policy", &policy);
    let mut manager = CoordinationPlacementManager::new(
        next_life.clone(),
        ScriptedTopology(Mutex::new(vec![view])),
        ScriptedPolicy(Mutex::new(vec![policy])),
        NoProvisioning,
    );
    let report = within("resuming reconcile", manager.reconcile())
        .await
        .expect("a report");
    artifacts.report("journalled retry", &report);
    assert!(
        report.reason.contains("journalled membership request"),
        "the journalled request resolved before any new plan: {}",
        report.reason
    );
    assert!(
        report
            .reason
            .contains(&format!("outcome {membership_outcome}")),
        "the redelivered outcome is the saved one: {}",
        report.reason
    );

    // The group holds the intent those exact bytes named — the operation id
    // the dead manager minted, never one from a fresh plan.
    let deadline = std::time::Instant::now() + PATIENCE;
    let proved: &str = loop {
        let status = within("status", next_life.reads().group_status())
            .await
            .expect("the status cut");
        if let Some(pending) = &status.pending_operation_id {
            assert_eq!(
                pending.as_slice(),
                operation_id.as_slice(),
                "the group's intent is the journalled operation id"
            );
            break "the group holds the journalled operation id as its active intent";
        }
        let mut positions: Vec<usize> = status
            .voters
            .iter()
            .map(|n| {
                corridor.position_of(&CoordinationNodeId::from_slice(&n.node_id).expect("32 bytes"))
            })
            .collect();
        positions.sort_unstable();
        if positions == desired_positions.to_vec() {
            // The intent converged before this poll; the committed transition
            // is exactly the one the journalled bytes asked for.
            break "the journalled transition committed before this poll";
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the journalled request never reached the group; voters are {positions:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    artifacts.record("journalled retry reached the group", proved);

    corridor.shutdown().await;
}

/// **Losing two voters produces status only.** With one of three left there is
/// no quorum, so the reconciliation reports and performs no local or forced
/// reconfiguration — topology is never authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn losing_two_corridor_voters_produces_status_only() {
    let artifacts = Artifacts::new("corridor-quorum-loss");
    let mut corridor = Corridor::start(artifacts.clone()).await;
    let admin = corridor.admin(&artifacts.journal("admin")).await;
    let before = corridor.committed_voters(&admin).await;

    corridor.hosts[1]
        .take()
        .expect("voter 1 is running")
        .shutdown()
        .await;
    corridor.hosts[2]
        .take()
        .expect("voter 2 is running")
        .shutdown()
        .await;

    let live: BTreeSet<usize> = (0..TOTAL).filter(|p| *p != 1 && *p != 2).collect();
    let view = evidence(
        &corridor.ids,
        &[CLUSTER_A, CLUSTER_B, CLUSTER_C],
        &live,
        lowest_bridge,
        true,
    );
    artifacts.topology("evidence after quorum loss", &view);
    let policy = corridor_policy(&corridor, 1, true, Duration::from_millis(1), &[]);
    artifacts.policy("quorum-loss policy", &policy);

    let mut manager = CoordinationPlacementManager::new(
        admin.clone(),
        ScriptedTopology(Mutex::new(vec![view])),
        // Every eligible replacement is seeded and every timer is already
        // crossed by a one-millisecond grace: if anything could make this
        // manager act, it would.
        ScriptedPolicy(Mutex::new(vec![policy])),
        NoProvisioning,
    );
    let report = within("no-quorum reconcile", manager.reconcile())
        .await
        .expect("a report, not an error");
    artifacts.report("quorum loss", &report);
    assert!(
        report.pending_operation_id.is_none(),
        "nothing was submitted: {}",
        report.reason
    );
    assert!(
        report.selected_voters.is_empty(),
        "no selection was even made: {:?}",
        short_set(report.selected_voters.iter().copied())
    );
    assert!(
        report.reason.contains("no leader or no quorum"),
        "the report names the stop: {}",
        report.reason
    );

    // The surviving voter's own committed membership is untouched: no local
    // reconfiguration happened behind the report.
    let survivor = corridor.host(0).coordination().expect("hosted");
    let mut committed: Vec<usize> = survivor
        .committed_nodes_for_testing()
        .iter()
        .map(|n| corridor.position_of(n))
        .collect();
    committed.sort_unstable();
    assert_eq!(
        committed, before,
        "the survivor forced no membership change of its own"
    );
    assert!(
        !survivor.membership_is_joint_for_testing(),
        "and left no joint configuration behind"
    );

    corridor.shutdown().await;
}

// ── The pure planner over every corridor input ordering ─────────────────────

/// The pure half's authority: a real signing key, so every fixture permit
/// carries a genuine signature the planner's full `verify_join_permit` — the
/// same one apply runs — accepts.
const PURE_SECRET: [u8; 32] = [0xB4; 32];
const PURE_DOMAIN: &str = "corridor.pure";
const PURE_GENESIS: [u8; 32] = [0x5C; 32];

fn pure_authority_key() -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(&PURE_SECRET)
        .verifying_key()
        .to_bytes()
}

fn pure_group() -> CoordinationGroupId {
    CoordinationGroupId::from_slice(&[0xC7; 32]).expect("32 bytes")
}

fn pure_node(tag: u8) -> CoordinationNodeId {
    CoordinationNodeId::from_slice(&[tag; 32]).expect("32 bytes")
}

fn pure_permit(id: CoordinationNodeId) -> Vec<u8> {
    use aster::coordination::contract::payload_fory;
    use aster::coordination::model::authority::{join_signing_bytes, signed_document_from_parts};
    use aster::coordination::raft::{JoinPermitV1, NodeIdV1};
    use ed25519_dalek::Signer;

    let permit = JoinPermitV1 {
        format_version: 1,
        group_id: pure_group().as_bytes().to_vec(),
        authority_domain: PURE_DOMAIN.to_string(),
        genesis_hash: PURE_GENESIS.to_vec(),
        candidate_node_id: id.as_bytes().to_vec(),
        candidate_ticket: canonical_ticket(id.as_bytes()),
        bootstrap_senders: vec![NodeIdV1 {
            node_id: vec![0x01; 32],
        }],
        join_nonce: vec![0x77; 32],
    };
    let document = payload_fory::<JoinPermitV1>()
        .serialize(&permit)
        .expect("the permit encodes");
    let signature =
        ed25519_dalek::SigningKey::from_bytes(&PURE_SECRET).sign(&join_signing_bytes(&document));
    signed_document_from_parts(document, &pure_authority_key(), &signature.to_bytes())
        .expect("the envelope encodes")
}

fn pure_candidate(id: CoordinationNodeId) -> PlacementCandidate {
    PlacementCandidate {
        node_id: id,
        member_seed: Some(MemberSeedV1 {
            node_id: id.as_bytes().to_vec(),
            ticket: canonical_ticket(id.as_bytes()),
            signed_join_permit: Some(pure_permit(id)),
        }),
    }
}

/// A snapshot over an explicitly ordered cluster list and far-pair list, so
/// the ordering sweep can vary exactly those and nothing else.
fn pure_snapshot(
    clusters: &[Vec<CoordinationNodeId>],
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

/// **The corridor's pure planner cases, over every input ordering.**
///
/// The corridor shape — ten producers, 4 / 3 / 3, three voters all starting in
/// cluster A, `minimum_separated_voter_pairs = 3` — driven directly through
/// the planner with **every** input collection permuted: the cluster list
/// (all six orderings), the members inside each cluster (forward and
/// reversed), the corroborated far-pair list (forward and reversed), the
/// eligible record (forward, reversed, and every rotation), and the current
/// voter set's construction order (every rotation). 6 × 2 × 2 × 12 × 3 = 864
/// assembled inputs, all of which must yield one recommendation, formatted
/// identically.
///
/// The two corridor cases are run under the same sweep: the initial spread
/// (an authorized rebalance from an all-in-one-cluster membership) and the
/// repair (one grace-crossed voter, the other two fixed).
#[test]
fn the_corridor_planner_is_independent_of_every_input_ordering() {
    let nodes: Vec<CoordinationNodeId> = (1..=10).map(|t| pure_node(0x20 + t)).collect();
    let clusters: Vec<Vec<CoordinationNodeId>> = vec![
        CLUSTER_A.iter().map(|p| nodes[*p]).collect(),
        CLUSTER_B.iter().map(|p| nodes[*p]).collect(),
        CLUSTER_C.iter().map(|p| nodes[*p]).collect(),
    ];
    let mut far: Vec<(CoordinationNodeId, CoordinationNodeId)> = Vec::new();
    for (i, left) in clusters.iter().enumerate() {
        for right in &clusters[i + 1..] {
            for a in left {
                for b in right {
                    far.push((*a, *b));
                }
            }
        }
    }
    let eligible_positions: Vec<usize> = ELIGIBLE.to_vec();
    let current: Vec<CoordinationNodeId> = (0..VOTERS).map(|p| nodes[p]).collect();

    // The six orderings of three clusters.
    const CLUSTER_ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];

    for (case, grace_crossed, rebalance, minimum) in [
        ("initial spread", None, true, 3u8),
        ("repair", Some(nodes[4]), false, 1u8),
    ] {
        let mut answers: BTreeSet<String> = BTreeSet::new();
        // The repair case starts from the spread membership, not the genesis
        // one: it is the corridor's *second* pass.
        let case_current: Vec<CoordinationNodeId> = if grace_crossed.is_some() {
            vec![nodes[0], nodes[4], nodes[6]]
        } else {
            current.clone()
        };

        for order in CLUSTER_ORDERS {
            for members_reversed in [false, true] {
                for far_reversed in [false, true] {
                    for eligible_rotation in 0..eligible_positions.len() {
                        for eligible_reversed in [false, true] {
                            for current_rotation in 0..case_current.len() {
                                let ordered: Vec<Vec<CoordinationNodeId>> = order
                                    .iter()
                                    .map(|i| {
                                        let mut members = clusters[*i].clone();
                                        if members_reversed {
                                            members.reverse();
                                        }
                                        members
                                    })
                                    .collect();
                                let mut pairs = far.clone();
                                if far_reversed {
                                    pairs.reverse();
                                }
                                let topology = pure_snapshot(&ordered, &pairs);

                                let mut positions = eligible_positions.clone();
                                positions.rotate_left(eligible_rotation);
                                if eligible_reversed {
                                    positions.reverse();
                                }
                                let mut eligible: BTreeMap<CoordinationNodeId, PlacementCandidate> =
                                    BTreeMap::new();
                                for position in positions {
                                    let id = nodes[position];
                                    eligible.insert(id, pure_candidate(id));
                                }

                                let mut voters = case_current.clone();
                                voters.rotate_left(current_rotation);

                                let input = PlacementInput {
                                    group_id: pure_group(),
                                    genesis_hash: PURE_GENESIS,
                                    deployment_profile_code: 1,
                                    membership_log_id: None,
                                    current_voters: voters.into_iter().collect(),
                                    current_learners: BTreeSet::new(),
                                    active_membership_intent: false,
                                    grace_crossed_members: grace_crossed.into_iter().collect(),
                                    policy: PlacementPolicy {
                                        revision: 1,
                                        digest: [0xD5; 32],
                                        authority_public_key: pure_authority_key(),
                                        authority_domain: PURE_DOMAIN.to_string(),
                                        target_voters: 3,
                                        target_retained_learners: 0,
                                        eligible,
                                        pinned_voters: BTreeSet::new(),
                                        excluded: BTreeSet::new(),
                                        minimum_separated_voter_pairs: minimum,
                                        replacement_grace: Duration::from_secs(600),
                                        allow_healthy_rebalance: rebalance,
                                    },
                                    topology,
                                };
                                answers.insert(format!("{:?}", plan(&input)));
                            }
                        }
                    }
                }
            }
        }

        let distinct: Vec<String> = answers.into_iter().collect();
        assert_eq!(
            distinct.len(),
            1,
            "the {case} case gave {} different answers across input orderings:\n{}",
            distinct.len(),
            distinct.join("\n---\n")
        );
        assert!(
            distinct[0].starts_with("Plan("),
            "the {case} case must produce a plan: {}",
            distinct[0]
        );
    }
}
