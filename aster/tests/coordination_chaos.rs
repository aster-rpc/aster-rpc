//! §S3-F: a real group driven through a **seeded, deterministic** faulty
//! network — drops, delays, duplicates, reorders, partitions and lost
//! leadership — with the seed recorded on every failure.
//!
//! ## The schedule is shared; the injectors are not
//!
//! Every voter runs its own [`ChaosNetwork`], because ordinals and the applied
//! tally are per-sender. They all run the **same** [`Schedule`], and that is
//! load-bearing rather than tidy: a partition is a property of a *pair*, so if
//! two voters drew from different seeds one could believe it was cut off from a
//! peer that still believed it was connected. That is a one-way link, which is
//! a different fault from a partition and one no consensus protocol promises
//! anything about. One seed makes `same_side` symmetric by construction.
//!
//! ## What makes these gates non-vacuous
//!
//! A fault schedule that happened to deliver everything would let a broken
//! implementation pass, and nothing in the run would say so. Each gate
//! therefore asserts against [`FaultTally`] — the count of what was *actually*
//! applied — and requires that every kind fired before it accepts a success.
//! A seed is recorded in every panic message. Note what that does and does not
//! buy: the schedule is a pure function, so replaying a seed reproduces the
//! verdict for message *n* to a given peer — but a live cluster's message
//! *count* depends on timing, so a rerun is not the same run. The seed narrows
//! an investigation and makes the schedule itself auditable; it is not a
//! promise that a failure repeats on demand. The pure-function property is
//! pinned separately by the unit tests in `network::chaos`.
//!
//! ## Why this drives the public surface
//!
//! The injector lives in the production Raft client, behind an `Option` that
//! only a `coordination-testing` build can populate. A harness that wrapped a
//! different client would prove things about the wrapper: the §5.3 admission
//! checks, the envelope encoding and the error classification are exactly what
//! a lossy network has to be survived *through*.

#![cfg(feature = "coordination-testing")]

use std::sync::Arc;
use std::time::Duration;

use aster::coordination::admission::{
    AdmissionEntryV1, AdmissionSetV1, SignedAdmissionSet, VerifiedAdmissionSource,
    ADMISSION_CLASS_PRODUCER, ADMISSION_FORMAT_VERSION,
};
use aster::coordination::contract::{
    ActionV1, ClientCommandV1, CommandReplyV1, CommandV1, ResourceKeyV1, SetGrantActionV1,
};
use aster::coordination::host::{CoordinationConfig, CoordinationStart};
use aster::coordination::model::authority::{
    canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts,
};
use aster::coordination::raft::{GenesisV1, NodeIdV1, VoterSeedV1};
use aster::coordination::testing::{ChaosNetwork, FaultTally, Schedule};
use aster::rpc::AsterServer;
use aster::{AsterConfig, RelayMode, SecretKey};
use ed25519_dalek::{Signer, SigningKey};

const DOMAIN: &str = "chaos.test";
const GROUP: [u8; 32] = [0xF0u8; 32];

/// The recorded seed. Changing it is changing the test: a different seed is a
/// different network, and a gate that passes only on one seed has found a bug
/// worth keeping rather than a seed worth replacing.
const SEED: u64 = 0x5EED_C0FF_EE00_1234;

/// Out of 100. High enough that every kind fires within a run of this length,
/// low enough that a majority still makes progress — a network that loses most
/// messages tests a partition, and there is a separate gate for that.
///
/// The rate and [`COMMIT_ROUNDS`] are chosen together against the rarest kind.
/// A duplicate is one eighth of the faulty share, so at this rate roughly one
/// message in 36 is duplicated: a run of a couple of dozen messages would
/// plausibly contain none, and the gate would then be asserting nothing about
/// duplicates while appearing to. Sizing the run is the fix; loosening the
/// assertion would only hide the same gap.
const FAULT_PERCENT: u8 = 28;

/// Commits driven through the faulty network. Each is a proposal plus its
/// replication to two peers, so this is the run's message volume.
const COMMIT_ROUNDS: u8 = 20;

/// Generous: every retry under this schedule costs at least one election
/// timeout, and `election_timeout_max` is 3 s. A hang still fails rather than
/// waits forever.
const PATIENCE: Duration = Duration::from_secs(90);

fn authority() -> SigningKey {
    SigningKey::from_bytes(&[0x91u8; 32])
}

fn authority_key() -> [u8; 32] {
    authority().verifying_key().to_bytes()
}

fn node_id_of(secret: [u8; 32]) -> Vec<u8> {
    SigningKey::from_bytes(&secret)
        .verifying_key()
        .to_bytes()
        .to_vec()
}

fn seed_ticket(node_id: &[u8]) -> String {
    aster_transport_core::ticket::AsterTicket {
        endpoint_id: node_id.to_vec().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a canonical ticket")
}

fn signed_genesis(secrets: &[[u8; 32]]) -> Vec<u8> {
    let mut initial_voters: Vec<VoterSeedV1> = secrets
        .iter()
        .map(|secret| {
            let node_id = node_id_of(*secret);
            VoterSeedV1 {
                ticket: seed_ticket(&node_id),
                node_id,
            }
        })
        .collect();
    initial_voters.sort_by(|l, r| l.node_id.cmp(&r.node_id));

    let mut initial_admins: Vec<NodeIdV1> = secrets
        .iter()
        .map(|secret| NodeIdV1 {
            node_id: node_id_of(*secret),
        })
        .collect();
    initial_admins.sort_by(|l, r| l.node_id.cmp(&r.node_id));

    let genesis = GenesisV1 {
        format_version: 1,
        group_id: GROUP.to_vec(),
        authority_domain: DOMAIN.into(),
        genesis_nonce: vec![0xA5u8; 32],
        deployment_profile_code: 1,
        initial_voters,
        initial_admins,
        recovery_export_hash: None,
    };
    let document = canonical_document_bytes(&genesis).expect("encode");
    let signature = authority().sign(&genesis_signing_bytes(&document));
    signed_document_from_parts(document, &authority_key(), &signature.to_bytes()).expect("encode")
}

fn admission_source(secrets: &[[u8; 32]]) -> Arc<dyn VerifiedAdmissionSource> {
    let mut entries: Vec<AdmissionEntryV1> = secrets
        .iter()
        .map(|secret| AdmissionEntryV1 {
            node_id: node_id_of(*secret),
            class_code: ADMISSION_CLASS_PRODUCER,
        })
        .collect();
    entries.sort_by(|l, r| l.node_id.cmp(&r.node_id));

    let set = AdmissionSetV1 {
        format_version: ADMISSION_FORMAT_VERSION,
        authority_domain: DOMAIN.into(),
        revision: 1,
        entries,
    };
    let document = canonical_document_bytes(&set).expect("encode");
    let signature = authority().sign(&aster::coordination::admission::admission_signing_bytes(
        &document,
    ));
    let signed = signed_document_from_parts(document, &authority_key(), &signature.to_bytes())
        .expect("encode");
    Arc::new(SignedAdmissionSet::verify(&authority_key(), DOMAIN, &signed).expect("a signed set"))
}

/// One running voter, its directory, and the injector its client consults.
struct ChaoticVoter {
    server: AsterServer,
    chaos: Arc<ChaosNetwork>,
    _dir: tempfile::TempDir,
}

/// A group of `secrets`, every voter behind the same schedule.
async fn start_chaotic_group(secrets: &[[u8; 32]], schedule: Schedule) -> Vec<ChaoticVoter> {
    let genesis = signed_genesis(secrets);
    let admission = admission_source(secrets);

    let mut voters = Vec::new();
    for secret in secrets {
        let dir = tempfile::tempdir().expect("a storage directory");
        let chaos = ChaosNetwork::new(schedule);
        let config = CoordinationConfig::builder()
            .storage_dir(dir.path())
            .authority_domain(DOMAIN)
            .start_mode(CoordinationStart::Bootstrap {
                signed_genesis: genesis.clone(),
                recovery_export_path: None,
            })
            .verified_admission(Arc::clone(&admission))
            .chaos_for_testing(Arc::clone(&chaos))
            .build()
            .expect("a complete configuration");
        let server = AsterServer::builder()
            .config(
                AsterConfig::builder()
                    .relay(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")
                    .secret_key(SecretKey::from_bytes(*secret))
                    .root_pubkey(aster::PublicKey::from_bytes(authority_key()))
                    .build(),
            )
            .builtin_coordination(config)
            .start()
            .await
            .expect("a voter starts");
        voters.push(ChaoticVoter {
            server,
            chaos,
            _dir: dir,
        });
    }

    let addrs: Vec<_> = voters.iter().map(|v| v.server.node().addr()).collect();
    for voter in &voters {
        for addr in &addrs {
            if addr.node_id != voter.server.node().addr().node_id {
                voter
                    .server
                    .node()
                    .add_node_addr(addr.clone())
                    .expect("register a peer address");
            }
        }
    }
    voters
}

/// The applied-fault totals across every voter.
fn combined_tally(voters: &[ChaoticVoter]) -> FaultTally {
    voters
        .iter()
        .map(|v| v.chaos.tally())
        .fold(FaultTally::default(), |mut acc, t| {
            acc.delivered += t.delivered;
            acc.dropped += t.dropped;
            acc.delayed += t.delayed;
            acc.duplicated += t.duplicated;
            acc.reordered += t.reordered;
            acc.partitioned += t.partitioned;
            acc
        })
}

/// Wait for a quorum-acknowledged leader, reporting the seed if none appears.
async fn await_leader(voters: &[ChaoticVoter], what: &str) -> usize {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        for (index, voter) in voters.iter().enumerate() {
            let coordination = voter.server.coordination().expect("hosted");
            if coordination.lifecycle_code() == 3 && coordination.quorum_available() == Some(true) {
                return index;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let observed: Vec<(i32, Option<bool>)> = voters
                .iter()
                .map(|v| {
                    let c = v.server.coordination().expect("hosted");
                    (c.lifecycle_code(), c.quorum_available())
                })
                .collect();
            panic!(
                "{what}: no leader within {PATIENCE:?} at seed {SEED:#x} \
                 (fault rate {FAULT_PERCENT}%); observed {observed:?}, \
                 faults {:?}",
                combined_tally(voters)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A grant on a fresh resource `key`, from **one** client at `sequence`.
///
/// One client id for the whole run, not one per command. A new client id per
/// round is not what a client does, and it is not free: §12.1 caps the active
/// sessions a group retains, so a twenty-round gate written that way stops
/// being about the network at round seven and starts being about the cap. That
/// is how this was first written, and `ActiveClientCap` said so.
///
/// A fixed `(client_id, sequence)` also makes the retry in [`commit_somewhere`]
/// meaningful rather than merely tolerated: a proposal whose reply the schedule
/// dropped is re-proposed as an **exact replay**, which the state machine must
/// answer from its saved result instead of applying twice.
fn grant(secret: &[u8; 32], client: &[u8], sequence: u64, key: &[u8]) -> CommandV1 {
    use aster::coordination::model::limits::{RIGHT_ACQUIRE, RIGHT_READ, RIGHT_RENEW};

    let action = ActionV1::SetGrant(SetGrantActionV1 {
        resource: ResourceKeyV1 {
            namespace: "chaos-gate".into(),
            key: key.to_vec(),
        },
        target_node_id: node_id_of(*secret),
        rights: RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW,
        // Each round grants on its own resource key, so every one is a fresh
        // ACL at revision 0.
        expected_acl_revision: 0,
    });
    CommandV1::Client(ClientCommandV1 {
        actor_node_id: node_id_of(*secret),
        client_id: aster::coordination::testing::id16(client),
        sequence,
        request_hash: aster::coordination::hash::request_hash(&action)
            .expect("a canonical action hashes")
            .to_vec(),
        action,
    })
}

/// §5.2 outcome 6, reason 0: a grant was written.
const GRANT_UPDATED: i32 = 6;

fn outcome_of(committed: &aster::coordination::testing::Committed) -> (i32, i32) {
    match &committed.reply {
        CommandReplyV1::Client(reply) => (reply.outcome_code, reply.reason_code),
        other => panic!("a client command must answer with a Client reply, got {other:?}"),
    }
}

/// Commit `command` at whichever voter currently leads, retrying across a
/// leadership change.
///
/// Retry is not leniency here: under this schedule the leader that accepted a
/// proposal may lose leadership before it commits, and a client that gave up on
/// the first `ForwardToLeader` would be testing its own impatience.
async fn commit_somewhere(
    voters: &[ChaoticVoter],
    command: CommandV1,
    what: &str,
) -> aster::coordination::testing::Committed {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let leader = await_leader(voters, what).await;
        let coordination = voters[leader].server.coordination().expect("hosted");
        if let Ok(committed) = coordination
            .propose_for_testing_within(command.clone(), Duration::from_secs(20))
            .await
        {
            return committed;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: no commit within {PATIENCE:?} at seed {SEED:#x}; faults {:?}",
            combined_tally(voters)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// **A group elects, commits and survives lost leadership through a seeded
/// faulty network.**
///
/// Three voters, one recorded seed, every message subject to the schedule.
/// The gate requires a real commit *after* the elected leader is deliberately
/// stepped down, so the run covers election, replication and failover rather
/// than a single lucky window — and it requires the tally to show that drops,
/// delays, duplicates and reorders all actually happened, so a schedule that
/// silently delivered everything fails instead of passing quietly.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_group_makes_progress_through_a_seeded_faulty_network() {
    let secrets = [[0xA1u8; 32], [0xA2u8; 32], [0xA3u8; 32]];
    let voters = start_chaotic_group(&secrets, Schedule::new(SEED, FAULT_PERCENT)).await;

    // Election, through a network that is dropping and delaying votes.
    let leader = await_leader(&voters, "the first election under chaos").await;

    // Replication, likewise.
    let client = b"chaos-client";
    let mut sequence = 0u64;
    let mut next = |key: &[u8]| {
        sequence += 1;
        grant(&secrets[0], client, sequence, key)
    };

    let first = commit_somewhere(&voters, next(b"before"), "the first commit").await;
    assert_eq!(
        outcome_of(&first),
        (GRANT_UPDATED, 0),
        "a committed grant is accepted even through a faulty network (seed {SEED:#x})"
    );

    // Enough traffic for the schedule to be a schedule rather than a coin
    // flip. An election plus one commit is a couple of dozen messages, and at
    // this rate the rarer kinds would plausibly not come up at all. This is
    // exactly what the tally assertion is for: the first two versions of this
    // gate ran too little traffic, and the tally said so instead of passing.
    for round in 0..COMMIT_ROUNDS {
        let key = [b'r', round];
        let committed = commit_somewhere(&voters, next(&key), "a mid-run commit").await;
        assert_eq!(
            outcome_of(&committed),
            (GRANT_UPDATED, 0),
            "round {round} commits (seed {SEED:#x})"
        );
    }

    // Lost leadership, at a moment the schedule did not choose — the phase
    // §S3-F names last, and the one a purely message-level fault never reaches.
    voters[leader]
        .server
        .coordination()
        .expect("hosted")
        .raft_for_testing()
        .trigger()
        .elect()
        .await
        .expect("an election can be triggered");

    // And progress again afterwards, which is the actual claim: the group
    // recovered rather than merely survived.
    let second = commit_somewhere(&voters, next(b"after"), "a commit after lost leadership").await;
    assert_eq!(
        outcome_of(&second),
        (GRANT_UPDATED, 0),
        "the group commits again after leadership moved (seed {SEED:#x})"
    );

    // Non-vacuity. Without this the gate would pass just as well on a perfect
    // network, and would therefore prove nothing about a faulty one.
    let tally = combined_tally(&voters);
    println!("seed {SEED:#x} at {FAULT_PERCENT}% applied {tally:?}");
    assert!(
        tally.every_kind_fired(),
        "the schedule must have applied every kind of fault, or this gate proves \
         nothing about a faulty network; seed {SEED:#x} produced {tally:?}"
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}

/// **A partition is stable within an epoch, and heals when the epoch moves.**
///
/// Distinct from the gate above because a partition is not a high drop rate:
/// dropping each message independently never leaves a *stable* minority, and a
/// group that only ever met independent drops would never exercise the case
/// where one side must refuse to commit for a sustained period.
///
/// Every voter advances its epoch together, because the epoch selects the sides
/// and two voters at different epochs would be a one-way link.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_partition_is_stable_within_an_epoch_and_heals_across_one() {
    let secrets = [[0xB1u8; 32], [0xB2u8; 32], [0xB3u8; 32]];
    let voters = start_chaotic_group(&secrets, Schedule::new(SEED ^ 0xBEEF, FAULT_PERCENT)).await;

    await_leader(&voters, "the election before any partition").await;
    let before = commit_somewhere(
        &voters,
        grant(&secrets[0], b"chaos-client", 1, b"pre-partition"),
        "a commit before the partition",
    )
    .await;
    assert_eq!(outcome_of(&before), (GRANT_UPDATED, 0));

    // Move every voter to the next epoch at once. The sides are re-drawn from
    // the shared seed, so all three agree on who can reach whom.
    for voter in &voters {
        voter.chaos.advance_epoch();
    }
    assert!(
        voters.iter().all(|v| v.chaos.epoch() == 1),
        "every voter must be at the same epoch, or the split is one-way"
    );

    // A majority side still commits. Which voters are on it is the schedule's
    // decision, not the test's, so the commit is asked of whoever leads.
    let after = commit_somewhere(
        &voters,
        grant(&secrets[0], b"chaos-client", 2, b"post-partition"),
        "a commit after the partition was re-drawn",
    )
    .await;
    assert_eq!(
        outcome_of(&after),
        (GRANT_UPDATED, 0),
        "a majority side commits across a partition (seed {:#x})",
        SEED ^ 0xBEEF
    );

    let tally = combined_tally(&voters);
    assert!(
        tally.partitioned > 0,
        "the partition must have actually dropped traffic; seed {:#x} produced {tally:?}",
        SEED ^ 0xBEEF
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}

/// **A perfect schedule changes nothing.**
///
/// The control. A zero fault rate must leave the group behaving exactly as the
/// unchaosed cluster gates do — if this fails, the injector itself is breaking
/// the group and every result above is about the harness rather than the code.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_zero_fault_schedule_leaves_the_group_untouched() {
    let secrets = [[0xC1u8; 32], [0xC2u8; 32], [0xC3u8; 32]];
    let voters = start_chaotic_group(&secrets, Schedule::perfect()).await;

    await_leader(&voters, "an election with no faults").await;
    let committed = commit_somewhere(
        &voters,
        grant(&secrets[0], b"chaos-client", 1, b"control"),
        "a commit with no faults",
    )
    .await;
    assert_eq!(outcome_of(&committed), (GRANT_UPDATED, 0));

    let tally = combined_tally(&voters);
    assert_eq!(
        (
            tally.dropped,
            tally.delayed,
            tally.duplicated,
            tally.reordered,
            tally.partitioned
        ),
        (0, 0, 0, 0, 0),
        "a perfect schedule applies no faults at all, and delivered {tally:?}"
    );
    assert!(
        tally.delivered > 0,
        "the injector must actually be on the path, or this control proves nothing"
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}

/// **A membership change completes through the faulty network.**
///
/// The phase §S3-F names that no message-level gate reaches. A membership
/// change is not one commit: the controller adds learners, waits for each to
/// catch up, enters a joint configuration and leaves it, and every one of those
/// steps is a separate round of replication that the schedule is dropping,
/// delaying, duplicating and reordering underneath.
///
/// Five voters shrinking to three, the two departing ones retained as learners
/// — the same shape `coordination_membership` proves on a healthy network, so
/// the difference between the two gates is the network and nothing else.
/// Profile 1 admits 3 or 5 voters and nothing between, which is why the change
/// is 5→3 rather than something smaller.
///
/// Completion is read from the **committed membership**, not from the
/// controller's reply: an accepted plan says the controller was asked, and this
/// gate is about whether it finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_membership_change_completes_through_a_seeded_faulty_network() {
    use std::collections::BTreeSet;

    // The **contract** `NodeIdV1`, not the `raft` one: a membership plan
    // travels as a client command, and the two types are distinct.
    use aster::coordination::contract::{
        ChangeMembershipActionV1, MemberSeedV1, NodeIdV1 as PlanNodeId,
    };

    let secrets = [
        [0xD1u8; 32],
        [0xD2u8; 32],
        [0xD3u8; 32],
        [0xD4u8; 32],
        [0xD5u8; 32],
    ];
    let voters = start_chaotic_group(&secrets, Schedule::new(SEED ^ 0x5171, FAULT_PERCENT)).await;
    // Wait for a first leader before planning: a plan proposed into a group
    // that has not elected is refused for a reason that is about the election
    // rather than about the network.
    await_leader(&voters, "the election before a membership change").await;

    // Sorted unique by node id, which is a public key: fixture order and byte
    // order are unrelated, and an unsorted plan is refused as shapeless before
    // any controller sees it.
    let named = |indices: &[usize]| {
        let mut ids: Vec<PlanNodeId> = indices
            .iter()
            .map(|i| PlanNodeId {
                node_id: node_id_of(secrets[*i]),
            })
            .collect();
        ids.sort_by(|l, r| l.node_id.cmp(&r.node_id));
        ids
    };

    /// §5.2's `MembershipAccepted`.
    const MEMBERSHIP_ACCEPTED: i32 = 8;

    // The plan is rebuilt on **every** attempt, because two of its inputs go
    // stale under chaos and a fixed command would be refused rather than
    // retried: `expected_membership_log_id` must be the membership the group
    // currently has, and the actor must be a voter that is currently leading.
    // A `None` expectation is not a wildcard — it is a mismatch, and reason 13
    // is what the first version of this gate got.
    //
    // The `operation_id` is fixed, so a plan that committed while its reply was
    // dropped is the *same* intent when it arrives again rather than a second
    // one racing the first.
    let plan_at = |leader: usize, expected| {
        let action = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
            operation_id: aster::coordination::testing::id16(b"chaos-shrink"),
            expected_membership_log_id: expected,
            desired_voters: named(&[0, 1, 2]),
            retained_learners: named(&[3, 4]),
            // Every node named is already a member, so none needs a route seed.
            member_seeds: Vec::<MemberSeedV1>::new(),
        });
        CommandV1::Client(ClientCommandV1 {
            actor_node_id: node_id_of(secrets[leader]),
            client_id: aster::coordination::testing::id16(b"chaos-admin"),
            sequence: 1,
            request_hash: aster::coordination::hash::request_hash(&action)
                .expect("a canonical action hashes")
                .to_vec(),
            action,
        })
    };

    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let at = await_leader(&voters, "a leader to accept the membership plan").await;
        let coordination = voters[at].server.coordination().expect("hosted");
        let expected = coordination.membership_log_id_for_testing();
        match coordination
            .propose_for_testing_within(plan_at(at, expected), Duration::from_secs(20))
            .await
        {
            Ok(committed) if outcome_of(&committed) == (MEMBERSHIP_ACCEPTED, 0) => break,
            // A refusal here is not a failure of the gate: leadership or the
            // membership log id can move between reading it and committing,
            // and the honest response is to re-read both and ask again. What
            // would be a failure is never being accepted at all, which the
            // deadline below catches.
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the membership plan was never accepted within {PATIENCE:?} at seed {:#x}; \
             faults {:?}",
            SEED ^ 0x5171,
            combined_tally(&voters)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Settled, non-joint, and with the durable intent finished. A joint
    // configuration containing the desired voters is what a change looks like
    // *mid-flight*, and the committed shape reaches its final form one entry
    // before the controller records completion.
    let want_voters: BTreeSet<Vec<u8>> =
        [0, 1, 2].iter().map(|i| node_id_of(secrets[*i])).collect();
    let want_nodes: BTreeSet<Vec<u8>> = (0..5).map(|i| node_id_of(secrets[i])).collect();

    let settle_deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let mut settled = false;
        for voter in &voters {
            let c = voter.server.coordination().expect("hosted");
            let have_voters: BTreeSet<Vec<u8>> = c
                .committed_voters_for_testing()
                .into_iter()
                .map(|n| n.as_bytes().to_vec())
                .collect();
            let have_nodes: BTreeSet<Vec<u8>> = c
                .committed_nodes_for_testing()
                .into_iter()
                .map(|n| n.as_bytes().to_vec())
                .collect();
            if have_voters == want_voters
                && have_nodes == want_nodes
                && !c.membership_is_joint_for_testing()
                && !c.membership_intent_active_for_testing().await
            {
                settled = true;
                break;
            }
        }
        if settled {
            break;
        }
        assert!(
            tokio::time::Instant::now() < settle_deadline,
            "the membership change never settled within {PATIENCE:?} at seed {:#x}; \
             faults {:?}",
            SEED ^ 0x5171,
            combined_tally(&voters)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let tally = combined_tally(&voters);
    println!("membership under chaos applied {tally:?}");
    assert!(
        tally.dropped > 0 && tally.delayed > 0,
        "the controller must have run through real faults; seed {:#x} produced {tally:?}",
        SEED ^ 0x5171
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}
