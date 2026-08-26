//! §S4-G: the agreed topology scenario — **ten Aster nodes, exactly three
//! persistent voters**, addressed by NodeId with ephemeral ports.
//!
//! The ten are: three genesis voters (A, B, C), six non-voter consumers
//! (D–I), and one standby candidate (J) started in `Join` mode — reachable,
//! admitted, and deliberately **not** a member. The gate exercises, in order:
//!
//! 1. non-voter consumers using the group as clients — grants, leases, reads —
//!    with no consumer ever entering membership;
//! 2. leadership movement: the sitting leader shuts down, the survivors
//!    elect, and the consumers' clients follow the authenticated redirect to
//!    the new leader and keep committing;
//! 3. voter replacement: an administrator submits one revision-fenced
//!    membership plan replacing the lost voter with J, and the committed
//!    voter set returns to exactly three — never four, never ten.
//!
//! Every identity binds `127.0.0.1:0` — an ephemeral port — and every node is
//! reached by NodeId. `add_node_addr` is the same-host substitution for
//! discovery that every cluster gate uses; nothing pins a port and nothing
//! treats an endpoint as a host:port service.
//!
//! The scenario's ten are the group's own nodes. The ROOT administrator runs
//! on an **eleventh, transient** endpoint — the §8.1 break-glass identity is
//! deliberately not one of the fleet, and counting it among the ten would
//! misstate what the ten-node claim is about.

#![cfg(feature = "coordination-testing")]

use std::time::Duration;

use aster::coordination::client::CoordinationClient;
use aster::coordination::contract::{MemberSeedV1, NodeIdV1, ResourceKeyV1};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::model::limits::{
    RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE, RIGHT_RENEW,
};
use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;
use aster::{AsterConfig, Node, PublicKey, RelayMode, SecretKey};

const PATIENCE: Duration = Duration::from_secs(90);
const SEED: u8 = 0xF1;

async fn within<T, F: std::future::Future<Output = T>>(what: &str, f: F) -> T {
    match tokio::time::timeout(PATIENCE, f).await {
        Ok(value) => value,
        Err(_) => panic!("{what}: no answer within {PATIENCE:?}"),
    }
}

fn resource(key: &[u8]) -> ResourceKeyV1 {
    ResourceKeyV1 {
        namespace: "topology.ten".into(),
        key: key.to_vec(),
    }
}

fn candidate_secret() -> [u8; 32] {
    [SEED ^ 0x77; 32]
}

fn consumer_secret(index: usize) -> [u8; 32] {
    [SEED ^ (0x50 + index as u8); 32]
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

/// Register every already-started node's address with every other — the
/// same-host discovery substitution.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ten_nodes_three_voters_move_leadership_and_replace_a_voter() {
    let fixture = GroupFixture::new("topology.ten", SEED, 3).admitting(candidate_secret());

    // ── The three genesis voters ────────────────────────────────────────────
    let voter_dirs: Vec<tempfile::TempDir> = (0..3)
        .map(|_| tempfile::tempdir().expect("a directory"))
        .collect();
    let mut voters = Vec::new();
    for (index, dir) in voter_dirs.iter().enumerate() {
        voters.push(
            AsterServer::builder()
                .config(fixture.aster_config(index))
                .builtin_coordination(fixture.config(dir.path(), index))
                .start()
                .await
                .expect("a voter starts"),
        );
    }

    // ── The standby candidate J: Join mode, reachable, not a member ─────────
    let candidate_id = node_id_of(candidate_secret());
    let candidate_dir = tempfile::tempdir().expect("a directory");
    let candidate = AsterServer::builder()
        .config(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes(candidate_secret()))
                .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
                .build(),
        )
        .builtin_coordination(fixture.join_config(
            candidate_dir.path(),
            candidate_secret(),
            &canonical_ticket(&candidate_id),
            &[0, 1, 2],
        ))
        .start()
        .await
        .expect("the candidate starts joining");

    // ── The six consumers: plain Aster nodes, never members ─────────────────
    let mut consumer_nodes = Vec::new();
    for index in 0..6 {
        consumer_nodes.push(
            Node::start(
                AsterConfig::builder()
                    .relay(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")
                    .secret_key(SecretKey::from_bytes(consumer_secret(index)))
                    .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
                    .build(),
            )
            .await
            .expect("a consumer starts"),
        );
    }

    // Ten Aster nodes total, all with ephemeral ports, all cross-registered.
    {
        let mut servers: Vec<&AsterServer> = voters.iter().collect();
        servers.push(&candidate);
        let nodes: Vec<&Node> = consumer_nodes.iter().collect();
        assert_eq!(servers.len() + nodes.len(), 10, "the scenario is ten nodes");
        cross_register(&servers, &nodes);
    }

    for voter in &voters {
        within(
            "voter readiness",
            voter.coordination().expect("hosted").wait_ready(),
        )
        .await
        .expect("the group becomes ready");
    }
    assert_eq!(
        candidate.coordination().expect("hosted").lifecycle_code(),
        2,
        "the standby candidate is Joining, not a member"
    );

    // ── ROOT and the consumers' clients ─────────────────────────────────────
    let root_node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(SecretKey::from_bytes([SEED ^ 0xFF; 32]))
            .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
            .build(),
    )
    .await
    .expect("the ROOT client node starts");
    for voter in &voters {
        root_node
            .add_node_addr(voter.node().addr())
            .expect("register");
    }
    let root_journal = tempfile::tempdir().expect("a directory");
    let seed_tickets: Vec<String> = (0..3)
        .map(|i| {
            let id: [u8; 32] = fixture.node_id(i).try_into().expect("32 bytes");
            canonical_ticket(&id)
        })
        .collect();
    let root = CoordinationClient::builder(root_node, fixture.group_id())
        .seed_tickets(seed_tickets.clone())
        .journal_dir(root_journal.path())
        .group_authority(fixture.authority_key(), fixture.domain())
        .build()
        .expect("the ROOT client builds");

    // Grants for two consumers on their own registers.
    let mut consumer_clients = Vec::new();
    let consumer_journals: Vec<tempfile::TempDir> = (0..2)
        .map(|_| tempfile::tempdir().expect("a directory"))
        .collect();
    for index in 0..2 {
        let subject = node_id_of(consumer_secret(index));
        let granted = within(
            "grant",
            root.set_grant(
                resource(format!("consumer-{index}").as_bytes()),
                subject,
                RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
                0,
            ),
        )
        .await
        .expect("ROOT grants");
        assert_eq!(granted.reply().outcome_code, 6);
        within("ack", granted.acknowledge()).await.expect("ack");

        let client = CoordinationClient::builder(consumer_nodes[index].clone(), fixture.group_id())
            .seed_tickets(seed_tickets.clone())
            .journal_dir(consumer_journals[index].path())
            .build()
            .expect("a consumer client builds");
        consumer_clients.push(client);
    }

    // ── Phase 1: consumers use the group ────────────────────────────────────
    for (index, client) in consumer_clients.iter().enumerate() {
        let acquired = within(
            "consumer acquire",
            client.acquire(resource(format!("consumer-{index}").as_bytes()), 30_000),
        )
        .await
        .expect("a consumer acquires");
        assert_eq!(acquired.reply().outcome_code, 1);
        within("ack", acquired.acknowledge()).await.expect("ack");
    }

    // ── Phase 2: leadership movement ────────────────────────────────────────
    let leader_id = voters[0]
        .coordination()
        .expect("hosted")
        .current_leader_for_testing()
        .expect("a ready group leads");
    let leader_index = (0..3)
        .find(|i| {
            CoordinationNodeId::from_slice(&fixture.node_id(*i)).expect("a node id") == leader_id
        })
        .expect("the leader is a fixture voter");
    let lost_voter_id = fixture.node_id(leader_index);
    voters.remove(leader_index).shutdown().await;

    // The consumers keep committing through the survivors: the client follows
    // the redirect to whoever now leads.
    let post_failover = within(
        "post-failover mutation",
        consumer_clients[0].compare_and_set(
            {
                let read = within(
                    "read for fence",
                    consumer_clients[0]
                        .reads()
                        .read_linearizable(resource(b"consumer-0")),
                )
                .await
                .expect("a linearizable read after failover");
                aster::coordination::contract::FenceV1 {
                    group_id: read.snapshot.group_id.clone(),
                    resource: read.snapshot.resource.clone(),
                    epoch: read.snapshot.epoch,
                    holder: read.snapshot.holder.clone().expect("held"),
                }
            },
            0,
            b"after-failover".to_vec(),
        ),
    )
    .await
    .expect("the group commits after leadership moved");
    assert_eq!(post_failover.reply().outcome_code, 3, "Mutated");
    within("ack", post_failover.acknowledge())
        .await
        .expect("ack");

    // ── Phase 3: voter replacement, without expanding the voter set ─────────
    let status = within("group_status", root.reads().group_status())
        .await
        .expect("ROOT reads the status cut");
    assert_eq!(
        status.voters.len(),
        3,
        "the committed set still names three"
    );
    let expected_membership = status.membership_log_id.clone();

    let mut desired: Vec<NodeIdV1> = (0..3)
        .filter(|i| *i != leader_index)
        .map(|i| NodeIdV1 {
            node_id: fixture.node_id(i),
        })
        .collect();
    desired.push(NodeIdV1 {
        node_id: candidate_id.to_vec(),
    });
    desired.sort_by(|l, r| l.node_id.cmp(&r.node_id));

    let member_seeds = vec![MemberSeedV1 {
        node_id: candidate_id.to_vec(),
        ticket: canonical_ticket(&candidate_id),
        signed_join_permit: Some(fixture.join_permit_with_ticket(
            candidate_secret(),
            &canonical_ticket(&candidate_id),
            &[0, 1, 2],
        )),
    }];

    let accepted = within(
        "change_membership",
        root.change_membership(
            *blake3::hash(b"replace-the-lost-voter").as_bytes()[..16]
                .first_chunk::<16>()
                .expect("16 bytes"),
            expected_membership,
            desired.clone(),
            Vec::new(),
            member_seeds,
        ),
    )
    .await
    .expect("the plan is accepted");
    assert_eq!(
        accepted.reply().outcome_code,
        8,
        "MembershipAccepted: {:?}",
        accepted.reply()
    );
    within("ack", accepted.acknowledge()).await.expect("ack");

    // The candidate graduates to a committed voter and reports Ready.
    within(
        "candidate readiness",
        candidate.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("the replacement voter becomes ready");

    // The committed set is exactly the desired three — the lost voter out,
    // the candidate in, no learner residue, and never ten.
    let deadline = std::time::Instant::now() + PATIENCE;
    let final_status = loop {
        let status = within("group_status", root.reads().group_status())
            .await
            .expect("the status cut");
        let voters_now: Vec<Vec<u8>> = status.voters.iter().map(|n| n.node_id.clone()).collect();
        let desired_now: Vec<Vec<u8>> = desired.iter().map(|n| n.node_id.clone()).collect();
        if voters_now == desired_now
            && status.learners.is_empty()
            && !status.membership_joint
            && status.pending_operation_id.is_none()
        {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the membership change never settled: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(
        final_status.voters.len(),
        3,
        "the settled uniform membership names exactly three"
    );
    assert!(
        !final_status
            .voters
            .iter()
            .any(|n| n.node_id == lost_voter_id),
        "the lost voter left the committed set"
    );
    assert!(
        final_status
            .voters
            .iter()
            .any(|n| n.node_id == candidate_id.to_vec()),
        "the standby candidate is now a voter"
    );

    // And the consumers' state survived the whole reshaping.
    let read = within(
        "final read",
        consumer_clients[0]
            .reads()
            .read_linearizable(resource(b"consumer-0")),
    )
    .await
    .expect("a linearizable read against the reshaped group");
    assert_eq!(
        read.snapshot.value.as_deref(),
        Some(b"after-failover".as_ref()),
        "no committed effect was lost across failover and replacement"
    );

    candidate.shutdown().await;
    for voter in voters {
        voter.shutdown().await;
    }
}
