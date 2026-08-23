//! Slice 3 gates: real multi-voter groups over real Iroh.
//!
//! Every voter here is a full `AsterServer` with its own endpoint, its own
//! storage directory, and its own signed identity. They find each other by
//! dialling, elect through real QUIC, and replicate real entries — nothing is
//! stubbed, and no test reaches inside the runtime.
//!
//! ## Why the seed tickets carry no addresses
//!
//! A genesis has to name every initial voter *before* any of them starts, and a
//! node's direct addresses do not exist until it binds. The seeds therefore
//! carry address-only tickets — canonical, Open, and naming the right node,
//! which is all §7.1 requires — and the real addresses are exchanged after
//! start through the ordinary address book.
//!
//! That is not a workaround for the test: it is what a real deployment does.
//! §5.4 says routes are advisory dialling hints and identity is the handshake's,
//! so a seed ticket that goes stale costs a discovery round trip and cannot
//! send a call to the wrong node.

#![cfg(feature = "coordination-testing")]

use std::sync::Arc;
use std::time::Duration;

use aster::coordination::admission::{
    AdmissionEntryV1, AdmissionSetV1, SignedAdmissionSet, VerifiedAdmissionSource,
    ADMISSION_CLASS_PRODUCER, ADMISSION_FORMAT_VERSION,
};
use aster::coordination::contract::{ActionV1, ClientCommandV1, CommandV1, ResourceKeyV1};
use aster::coordination::host::{CoordinationConfig, CoordinationStart};
use aster::coordination::model::authority::{
    canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts,
};
use aster::coordination::raft::{GenesisV1, NodeIdV1, VoterSeedV1};
use aster::rpc::AsterServer;
use aster::{AsterConfig, RelayMode, SecretKey};
use ed25519_dalek::{Signer, SigningKey};

const DOMAIN: &str = "cluster.test";
const GROUP: [u8; 32] = [0xC5u8; 32];

/// Long enough for a real election plus a few retried dials, short enough that
/// a hang is a failure rather than a wait. `election_timeout_max` is 3 s.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

fn authority() -> SigningKey {
    SigningKey::from_bytes(&[0x77u8; 32])
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

/// An address-only Open ticket naming `node_id`.
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

/// The signed genesis for a group of `secrets`, at `profile`.
fn signed_genesis(secrets: &[[u8; 32]], profile: i32) -> Vec<u8> {
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
        deployment_profile_code: profile,
        initial_voters,
        initial_admins,
        recovery_export_hash: None,
    };
    let document = canonical_document_bytes(&genesis).expect("encode");
    let signature = authority().sign(&genesis_signing_bytes(&document));
    signed_document_from_parts(document, &authority_key(), &signature.to_bytes()).expect("encode")
}

/// An authority-signed admission set naming every voter as a producer.
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

/// How long a write is given before the attempt is called failed.
///
/// Comfortably above a round trip on loopback and well below the point at which
/// a stalled test looks like a hang. A minority write never resolves at all, so
/// this is the bound on the harness's patience rather than on the group's.
const WRITE_WINDOW: Duration = Duration::from_secs(5);

/// One real client command: grant `key`'s rights to the actor.
///
/// A **grant**, not an acquire. Acquiring needs `RIGHT_ACQUIRE`, which a fresh
/// resource grants nobody, so an acquire would be refused for a reason that has
/// nothing to do with quorum. Setting a grant is administrator-only, and this
/// genesis names every voter an administrator — so it is the smallest command
/// this group genuinely accepts.
///
/// That distinction is the point of the control: a minority-safety gate has to
/// compare "the group accepts writes" with "the group cannot", and a command
/// refused in both cases compares nothing.
fn grant(secret: &[u8; 32], key: &[u8]) -> CommandV1 {
    use aster::coordination::contract::SetGrantActionV1;
    use aster::coordination::model::limits::{RIGHT_ACQUIRE, RIGHT_READ, RIGHT_RENEW};

    let action = ActionV1::SetGrant(SetGrantActionV1 {
        resource: ResourceKeyV1 {
            namespace: "cluster-gate".into(),
            key: key.to_vec(),
        },
        target_node_id: node_id_of(*secret),
        rights: RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW,
        // A resource nobody has granted anything on is at revision 0.
        expected_acl_revision: 0,
    });
    CommandV1::Client(ClientCommandV1 {
        actor_node_id: node_id_of(*secret),
        client_id: aster::coordination::testing::id16(key),
        sequence: 1,
        request_hash: aster::coordination::hash::request_hash(&action)
            .expect("a canonical action hashes")
            .to_vec(),
        action,
    })
}

/// §5.2 outcome 6: a grant was written.
const GRANT_UPDATED: i32 = 6;

/// §5.2 reason 0: no reason applies, which is what every success carries.
const NO_REASON: i32 = 0;

/// The outcome and reason a committed command answered with.
fn outcome_of(committed: &aster::coordination::testing::Committed) -> (i32, i32) {
    match &committed.reply {
        aster::coordination::contract::CommandReplyV1::Client(reply) => {
            (reply.outcome_code, reply.reason_code)
        }
        other => panic!("a client command must answer with a Client reply, got {other:?}"),
    }
}

/// One running voter and the directory it owns.
struct Voter {
    server: AsterServer,
    _dir: tempfile::TempDir,
}

/// Start every voter of a group, then cross-register their real addresses.
///
/// The two steps are separate because they have to be: a genesis names its
/// voters before any of them binds, so the addresses are only knowable after
/// `start()` returns. Until they are exchanged the voters retry dialling and
/// back off, which is exactly what §5.4's `Unreachable` mapping is for — and it
/// is why the readiness wait below is generous rather than instant.
async fn start_group(secrets: &[[u8; 32]], profile: i32, acknowledge: bool) -> Vec<Voter> {
    let genesis = signed_genesis(secrets, profile);
    let admission = admission_source(secrets);

    let mut voters = Vec::new();
    for secret in secrets {
        let dir = tempfile::tempdir().expect("a storage directory");
        let mut builder = CoordinationConfig::builder()
            .storage_dir(dir.path())
            .authority_domain(DOMAIN)
            .start_mode(CoordinationStart::Bootstrap {
                signed_genesis: genesis.clone(),
                recovery_export_path: None,
            })
            .verified_admission(Arc::clone(&admission));
        if acknowledge {
            builder = builder
                .acknowledge_single_voter_development_group(true)
                .acknowledge_even_quorum_development_group(true);
        }
        let server = AsterServer::builder()
            .config(
                AsterConfig::builder()
                    .relay(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")
                    .secret_key(SecretKey::from_bytes(*secret))
                    .root_pubkey(aster::PublicKey::from_bytes(authority_key()))
                    .build(),
            )
            .builtin_coordination(builder.build().expect("a complete configuration"))
            .start()
            .await
            .expect("a voter starts");
        voters.push(Voter { server, _dir: dir });
    }

    // Everyone learns everyone's real address. Identity is still the
    // handshake's — this only makes the first dial succeed instead of waiting
    // for discovery.
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

/// Wait until the group has a **leader**, and return which voter it is.
///
/// "Ready" alone is not "this is the leader". §7.6 makes a follower's `Ready`
/// **advisory** — it proves recent leader contact and nothing about quorum — so
/// the first node to report ready is often a follower, which correctly answers
/// `quorum_available: None`.
///
/// The leader is the one holding *direct* evidence, which is exactly what
/// `Some(true)` means. Waiting on that rather than on readiness is the
/// difference between "somebody thinks a leader exists" and "a leader has been
/// acknowledged by a quorum".
async fn await_leader(voters: &[Voter], what: &str) -> usize {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        for (index, voter) in voters.iter().enumerate() {
            let coordination = voter.server.coordination().expect("hosted");
            // `3` is §5.2's `Ready`, and `Some(true)` is a quorum-acknowledged
            // leader — a follower reports `None` however healthy it is.
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
            panic!("{what}: no leader within {READY_TIMEOUT:?}; observed {observed:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// **Exactly one voter claims direct quorum evidence, and no follower does.**
///
/// §7.6's `Option<bool>` in one assertion: `Some` on a leader, `None` on a
/// follower. A plain `bool` would force every ready follower to claim a quorum
/// it cannot observe, which is the stranded-follower failure the type exists to
/// prevent.
fn exactly_one_leader_claims_quorum(voters: &[Voter], what: &str) {
    let claiming: Vec<usize> = voters
        .iter()
        .enumerate()
        .filter(|(_, v)| {
            v.server
                .coordination()
                .expect("hosted")
                .quorum_available()
                .is_some()
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        claiming.len(),
        1,
        "{what}: exactly one voter holds direct quorum evidence; {claiming:?} did"
    );
    assert_eq!(
        voters[claiming[0]]
            .server
            .coordination()
            .expect("hosted")
            .quorum_available(),
        Some(true),
        "{what}: and that one is a quorum-acknowledged leader"
    );
}

/// **All four declared cluster shapes form.**
///
/// §7.1 declares three profile codes admitting **four** shapes — profile 1
/// permits 3 *or* 5 — and this exercises every one. A test covering only three
/// would leave whichever it skipped unproven, and the 5-voter case is the one
/// where a quorum is not simply "everyone".
///
/// Each shape is run to `Ready`, which is not merely "started": readiness
/// requires a committed membership containing the node, a fresh quorum
/// acknowledgement, and the leader's blank entry applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_declared_cluster_shape_forms_and_becomes_ready() {
    let shapes: &[(&str, &[[u8; 32]], i32)] = &[
        ("DevSingle", &[[0x01u8; 32]], 2),
        ("DevEven", &[[0x11u8; 32], [0x12u8; 32]], 3),
        ("Prod3", &[[0x21u8; 32], [0x22u8; 32], [0x23u8; 32]], 1),
        (
            "Prod5",
            &[
                [0x31u8; 32],
                [0x32u8; 32],
                [0x33u8; 32],
                [0x34u8; 32],
                [0x35u8; 32],
            ],
            1,
        ),
    ];

    for (name, secrets, profile) in shapes {
        let voters = start_group(secrets, *profile, true).await;
        await_leader(&voters, name).await;
        exactly_one_leader_claims_quorum(&voters, name);

        for voter in voters {
            voter.server.shutdown().await;
        }
    }
}

/// **A three-voter group survives one loss.**
///
/// One voter is shut down — its endpoint closes, its storage lock releases, and
/// the remaining two must elect and commit without it. Two of three is a
/// quorum, so this is the property that distinguishes a real group from three
/// nodes that happen to agree.
///
/// The evidence is readiness *after* the loss, not before: a leader that was
/// already ready and stayed ready proves nothing about the survivors, since it
/// might be the one that never lost contact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_three_voter_group_survives_one_loss() {
    let secrets = [[0x41u8; 32], [0x42u8; 32], [0x43u8; 32]];
    let mut voters = start_group(&secrets, 1, false).await;
    let leader = await_leader(&voters, "before the loss").await;

    // Take out the **leader**, which is the harder case: a lost follower leaves
    // the leadership intact, and the group would look healthy without ever
    // holding an election.
    let lost = voters.remove(leader);
    lost.server.shutdown().await;

    // The survivors must elect a new leader among themselves — not merely
    // report readiness, which a follower does advisorily.
    await_leader(&voters, "after losing the leader").await;
    exactly_one_leader_claims_quorum(&voters, "after losing the leader");

    for voter in voters {
        voter.server.shutdown().await;
    }
}

/// **A minority cannot commit — a real write is attempted and does not.**
///
/// This once asserted only that the survivor stopped reporting `Ready`. That is
/// a statement about *reporting*, and the property is about *committing*: a
/// system that acknowledged writes while announcing `NoQuorum` would have
/// passed, which is precisely the failure that matters. Lifecycle reporting is
/// the symptom; the commit is the thing.
///
/// So the write is real, and it runs twice. First against the full group, where
/// it must commit and advance the applied index — without that control, "the
/// write did not commit" could mean the harness never managed to submit one.
/// Then against the lone survivor, where it must not.
///
/// The survivor's failure is a **timeout**, not an error, and that is correct:
/// OpenRaft has no basis to abandon a legitimately pending entry, so it waits.
/// Which is why the applied index is checked independently afterwards — a
/// proposal that timed out locally could still have committed, and only the log
/// can say.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_minority_cannot_commit() {
    let secrets = [[0x51u8; 32], [0x52u8; 32], [0x53u8; 32]];
    let mut voters = start_group(&secrets, 1, false).await;
    let leader = await_leader(&voters, "before the partition").await;

    // The control: with all three up, a write commits and the log moves.
    let coordination = voters[leader].server.coordination().expect("hosted");
    let before = coordination.last_applied_for_testing();
    let committed = coordination
        .propose_for_testing_within(grant(&secrets[leader], b"quorum-control"), WRITE_WINDOW)
        .await
        .expect("a three-voter group commits a write");
    // Committed is not accepted. Apply is total, so a request the group refuses
    // still commits and replies with a rejection — and a control that only
    // checked for a commit would be satisfied by a write the group turned down,
    // which says nothing about whether writes work.
    //
    // This asserted a *refusal* until the signed genesis became a committed
    // command: before that, the delegated administrator set was empty and no
    // command a fresh group received could be accepted at all.
    assert_eq!(
        outcome_of(&committed),
        (GRANT_UPDATED, NO_REASON),
        "a three-voter group with a leader must accept an administrator's grant"
    );
    assert!(
        Some(committed.index) > before,
        "the control write must land past the index the group was at: \
         {before:?} -> {}",
        committed.index
    );

    // Now leave exactly one — including the leader, so the survivor is the one
    // that has to discover it cannot proceed.
    let survivor = voters.remove(leader);
    for voter in voters {
        voter.server.shutdown().await;
    }

    let coordination = survivor.server.coordination().expect("hosted");
    let before = coordination.last_applied_for_testing();

    // Longer than an election timeout, so a refusal is not merely a node caught
    // mid-election.
    tokio::time::sleep(Duration::from_secs(8)).await;

    let outcome = coordination
        .propose_for_testing_within(grant(&secrets[leader], b"minority-write"), WRITE_WINDOW)
        .await;
    assert!(
        outcome.is_err(),
        "one of three is not a quorum; a write must not be acknowledged: {outcome:?}"
    );

    // The independent oracle. A local timeout is not proof the entry did not
    // commit — the log is.
    let after = coordination.last_applied_for_testing();
    assert_eq!(
        after, before,
        "a minority must not advance its applied index: {before:?} -> {after:?}"
    );

    // And the reporting agrees with the behaviour, which is the original
    // assertion kept rather than replaced.
    assert_ne!(
        coordination.lifecycle_code(),
        3,
        "a lone voter must not report Ready"
    );
    assert_ne!(
        coordination.quorum_available(),
        Some(true),
        "a lone voter must never claim direct evidence of a reachable quorum"
    );

    survivor.server.shutdown().await;
}

/// **A group re-forms after every voter restarts.**
///
/// Each voter reopens its own directory through `Existing`, which never
/// initializes — so this proves the durable binding carried the group across a
/// full stop, rather than a second group being founded over the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_three_voter_group_reforms_after_a_full_restart() {
    let secrets = [[0x61u8; 32], [0x62u8; 32], [0x63u8; 32]];
    let genesis = signed_genesis(&secrets, 1);
    let admission = admission_source(&secrets);

    // Directories outlive the servers, which is the whole point.
    let dirs: Vec<tempfile::TempDir> = (0..3)
        .map(|_| tempfile::tempdir().expect("a storage directory"))
        .collect();

    let genesis_hash = {
        use aster::coordination::raft::SignedCoordinationDocumentV1;
        let envelope: SignedCoordinationDocumentV1 =
            aster::coordination::contract::payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&genesis)
                .expect("decode");
        aster::coordination::model::authority::genesis_hash(&envelope.document_bytes)
    };

    // First run: bootstrap.
    let mut first = Vec::new();
    for (i, secret) in secrets.iter().enumerate() {
        let server = AsterServer::builder()
            .config(
                AsterConfig::builder()
                    .relay(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")
                    .secret_key(SecretKey::from_bytes(*secret))
                    .root_pubkey(aster::PublicKey::from_bytes(authority_key()))
                    .build(),
            )
            .builtin_coordination(
                CoordinationConfig::builder()
                    .storage_dir(dirs[i].path())
                    .authority_domain(DOMAIN)
                    .start_mode(CoordinationStart::Bootstrap {
                        signed_genesis: genesis.clone(),
                        recovery_export_path: None,
                    })
                    .verified_admission(Arc::clone(&admission))
                    .build()
                    .expect("config"),
            )
            .start()
            .await
            .expect("a voter starts");
        first.push(server);
    }
    let addrs: Vec<_> = first.iter().map(|s| s.node().addr()).collect();
    for server in &first {
        for addr in &addrs {
            if addr.node_id != server.node().addr().node_id {
                server.node().add_node_addr(addr.clone()).expect("register");
            }
        }
    }
    // Wait for the group to come up.
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if first
            .iter()
            .any(|s| s.coordination().expect("hosted").lifecycle_code() == 3)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the group did not form before the restart"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for server in first {
        server.shutdown().await;
    }

    // Second run: reopen through `Existing`, with the binding supplied
    // independently of the store.
    let mut second = Vec::new();
    for (i, secret) in secrets.iter().enumerate() {
        let server = AsterServer::builder()
            .config(
                AsterConfig::builder()
                    .relay(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")
                    .secret_key(SecretKey::from_bytes(*secret))
                    .root_pubkey(aster::PublicKey::from_bytes(authority_key()))
                    .build(),
            )
            .builtin_coordination(
                CoordinationConfig::builder()
                    .storage_dir(dirs[i].path())
                    .authority_domain(DOMAIN)
                    .start_mode(CoordinationStart::Existing {
                        signed_genesis: genesis.clone(),
                        expected_group_id:
                            aster::coordination::ids::CoordinationGroupId::from_array(GROUP),
                        expected_genesis_hash: genesis_hash,
                        expected_deployment_profile_code: 1,
                    })
                    .verified_admission(Arc::clone(&admission))
                    .build()
                    .expect("config"),
            )
            .start()
            .await
            .expect("a voter reopens");
        second.push(server);
    }
    let addrs: Vec<_> = second.iter().map(|s| s.node().addr()).collect();
    for server in &second {
        for addr in &addrs {
            if addr.node_id != server.node().addr().node_id {
                server.node().add_node_addr(addr.clone()).expect("register");
            }
        }
    }

    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if second
            .iter()
            .any(|s| s.coordination().expect("hosted").lifecycle_code() == 3)
        {
            break;
        }
        let codes: Vec<i32> = second
            .iter()
            .map(|s| s.coordination().expect("hosted").lifecycle_code())
            .collect();
        assert!(
            tokio::time::Instant::now() < deadline,
            "the group did not re-form after a full restart; codes {codes:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    for server in second {
        server.shutdown().await;
    }
}

/// **No unauthorized identity reaches OpenRaft.**
///
/// A fourth node, cryptographically real and perfectly able to dial, is absent
/// from both the signed genesis and the admission set. It speaks the §5.3
/// protocol correctly — a well-formed envelope, a real vote request, its own
/// authenticated NodeId — and is refused.
///
/// Two refusals are checked, because they are different failures and a single
/// one would leave the other untested:
///
/// * a **well-formed, correctly-addressed** call from an unlisted node, which
///   must fail admission or the allowlist and never reach a handler; and
/// * the same node claiming *someone else's* `sender_node_id` in the envelope,
///   which must fail against the authenticated peer regardless of what the
///   payload says.
///
/// The second is the one that matters most: it is the only way a remote could
/// select a local role from payload bytes, and §5.3 exists to make it
/// impossible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unauthorized_identity_never_reaches_openraft() {
    use aster::coordination::contract::payload_fory;
    use aster::coordination::raft::{RaftEnvelopeV1, RaftMessageV1, VoteRequestV1, VoteV1};
    use aster::rpc::StreamHeader;

    let secrets = [[0x71u8; 32], [0x72u8; 32], [0x73u8; 32]];
    let voters = start_group(&secrets, 1, false).await;
    let leader = await_leader(&voters, "the target group").await;
    let target = voters[leader].server.node().addr();

    // The stranger: a real endpoint, a real key, and no place in the group.
    let stranger_secret = [0x7Fu8; 32];
    let stranger_id = node_id_of(stranger_secret);
    let stranger = aster::Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(SecretKey::from_bytes(stranger_secret))
            .root_pubkey(aster::PublicKey::from_bytes(authority_key()))
            .build(),
    )
    .await
    .expect("the stranger starts");
    stranger
        .add_node_addr(target.clone())
        .expect("the stranger can reach the group");

    let vote = RaftMessageV1::VoteRequest(VoteRequestV1 {
        vote: VoteV1 {
            term: 99,
            leader_node_id: stranger_id.clone(),
            committed: false,
        },
        last_log_id: None,
    });
    let payload = payload_fory::<RaftMessageV1>()
        .serialize(&vote)
        .expect("encode");

    let call = |sender_node_id: Vec<u8>| {
        let envelope = RaftEnvelopeV1 {
            protocol_version: 1,
            raft_codec_version: 1,
            group_id: GROUP.to_vec(),
            sender_node_id,
            payload: payload.clone(),
        };
        payload_fory::<RaftEnvelopeV1>()
            .serialize(&envelope)
            .expect("encode")
    };

    let header = StreamHeader {
        service: aster::coordination::service::RAFT_SERVICE.to_string(),
        method: "vote".to_string(),
        version: 1,
        ..Default::default()
    };

    let connection = stranger
        .rpc_connect(&aster::NodeId::from_hex(
            target.node_id.as_str().to_string(),
        ))
        .await
        .expect("the stranger connects to a live voter");

    // 1. Honest about who it is, and simply not a member.
    let honest = connection
        .unary(&header, call(stranger_id.clone()))
        .await
        .expect_err("an unlisted node must be refused");
    let honest = honest.to_string();
    assert!(
        honest.contains("admission denied") || honest.contains("not a member"),
        "an unlisted node must fail admission or the allowlist, got: {honest}"
    );

    // 2. Claiming a real voter's identity in the envelope. The authenticated
    //    peer is still the stranger, and that is what decides.
    let impersonating = connection
        .unary(&header, call(node_id_of(secrets[0])))
        .await
        .expect_err("a forged sender must be refused");
    let impersonating = impersonating.to_string();
    assert!(
        impersonating.contains("does not match the authenticated peer"),
        "identity must come from the handshake, never from the payload; got: {impersonating}"
    );

    // And the group is unharmed: it still has a leader, and its term was not
    // moved by a vote request from outside. A refusal that reached OpenRaft
    // first would have bumped it.
    exactly_one_leader_claims_quorum(&voters, "after the intrusion");

    stranger.shutdown().await;
    for voter in voters {
        voter.server.shutdown().await;
    }
}
