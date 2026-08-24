//! **The signed genesis, as a committed entry** (§7.1, §8.1, §S3-ROOT-AUTHORITY).
//!
//! ## The gap this closes
//!
//! `GenesisV1.initial_admins` was verified at start, bounded, required
//! non-empty — and then reached nothing. `boot::initialize` writes OpenRaft's
//! membership entry and nothing else, and `set_admin` requires an existing
//! administrator, so the delegated set could never become non-empty. A group
//! could form, elect, replicate, and never authorize an administrative action:
//! no grant, and therefore no membership change, forever.
//!
//! It was found by a membership gate, not by review, and it was invisible to
//! every test that checked only whether a command *committed* — because a
//! refused command commits too.
//!
//! ## Why a committed command rather than seeded state
//!
//! State written before Raft initialization is state a full-log replay does not
//! reproduce; state derived from locally loaded key material is state two
//! voters can disagree about. A committed entry is the only form every present
//! and future member obtains identically, by replay or by the snapshot that
//! subsumes it.
//!
//! ## ROOT
//!
//! `ASTER_ROOT_PUBKEY` is the deployment's trust anchor and *is* the identity
//! of the immutable super-administrator: a NodeId is a 32-byte public key, so
//! holding the ROOT secret and speaking as that endpoint is authenticating as
//! ROOT. Voters decide it from the replicated key and the authenticated actor,
//! never from a host-local "the root secret is loaded" flag — a flag is a
//! per-node opinion, and two voters with different opinions about who may
//! change membership is a split group.

#![cfg(feature = "coordination-testing")]

use std::time::Duration;

use aster::coordination::contract::{
    ActionV1, ClientCommandV1, CommandReplyV1, CommandV1, GenesisCommandV1, ResourceKeyV1,
    SetAdminActionV1, SetGrantActionV1,
};
use aster::coordination::testing::{Committed, GroupFixture};
use aster::rpc::AsterServer;

const DOMAIN: &str = "genesis.test";
const SEED: u8 = 0x77;
const PATIENCE: Duration = Duration::from_secs(60);

/// §5.2 codes, quoted rather than derived.
const REJECTED: i32 = 100;
const GRANT_UPDATED: i32 = 6;
const ADMIN_UPDATED: i32 = 7;
const NO_REASON: i32 = 0;
const ROOT_AUTHORITY_IMMUTABLE: i32 = 26;
const ADMIN_REVISION_MISMATCH: i32 = 7;
const PERMISSION_CHANGED: i32 = 12;
const SYS_GENESIS_ALREADY_INSTALLED: i32 = 8;

struct Voter {
    server: AsterServer,
    _dir: tempfile::TempDir,
}

impl Voter {
    fn coordination(&self) -> aster::coordination::Coordination {
        self.server.coordination().expect("hosted")
    }
}

/// A one-voter group, ready.
///
/// One voter because everything here is about *authorization state*, which is
/// replicated identically to every member: a three-voter fixture would prove
/// the same properties three times over and add an election to each.
async fn start(fixture: &GroupFixture) -> Voter {
    let dir = tempfile::tempdir().expect("a storage directory");
    let server = AsterServer::builder()
        .config(fixture.aster_config(0))
        .builtin_coordination(fixture.config(dir.path(), 0))
        .start()
        .await
        .expect("a voter starts");
    let voter = Voter { server, _dir: dir };
    tokio::time::timeout(PATIENCE, voter.coordination().wait_ready())
        .await
        .expect("ready in time")
        .expect("ready");
    voter
}

fn outcome_of(committed: &Committed) -> (i32, i32) {
    match &committed.reply {
        CommandReplyV1::Client(reply) => (reply.outcome_code, reply.reason_code),
        other => panic!("a client command must answer with a Client reply, got {other:?}"),
    }
}

fn system_code(committed: &Committed) -> i32 {
    match &committed.reply {
        CommandReplyV1::System(reply) => reply.system_result_code,
        other => panic!("a genesis must answer with a System reply, got {other:?}"),
    }
}

fn client(fixture: &GroupFixture, actor: usize, label: &str, action: ActionV1) -> CommandV1 {
    CommandV1::Client(ClientCommandV1 {
        actor_node_id: fixture.node_id(actor),
        client_id: aster::coordination::testing::id16(label.as_bytes()),
        sequence: 1,
        request_hash: aster::coordination::hash::request_hash(&action)
            .expect("a canonical action hashes")
            .to_vec(),
        action,
    })
}

fn set_grant(fixture: &GroupFixture, actor: usize, label: &str) -> CommandV1 {
    use aster::coordination::model::limits::{RIGHT_ACQUIRE, RIGHT_READ};
    client(
        fixture,
        actor,
        label,
        ActionV1::SetGrant(SetGrantActionV1 {
            resource: ResourceKeyV1 {
                namespace: "genesis-gate".into(),
                key: label.as_bytes().to_vec(),
            },
            target_node_id: fixture.node_id(actor),
            rights: RIGHT_READ | RIGHT_ACQUIRE,
            expected_acl_revision: 0,
        }),
    )
}

/// **Readiness waits for the genesis, and the delegated set is exactly the
/// genesis's.**
///
/// `wait_ready` returning is the assertion: §7.6 gained the genesis as a
/// readiness condition precisely so a caller is never told the group is usable
/// at the one moment every administrative action is guaranteed to be refused.
/// The grant that follows proves it is not merely *reported* installed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ready_voter_has_applied_its_genesis_and_can_authorize() {
    let fixture = GroupFixture::new(DOMAIN, SEED, 1);
    let voter = start(&fixture).await;
    let coordination = voter.coordination();

    assert!(
        coordination.genesis_installed_for_testing().await,
        "a voter that reports Ready has applied its genesis"
    );

    let committed = coordination
        .propose_for_testing_within(set_grant(&fixture, 0, "after-genesis"), PATIENCE)
        .await
        .expect("the grant commits");
    assert_eq!(
        outcome_of(&committed),
        (GRANT_UPDATED, NO_REASON),
        "the genesis's administrator can issue a grant"
    );

    voter.server.shutdown().await;
}

/// **Replaying the exact genesis changes nothing and says so.**
///
/// Idempotence is what makes the proposal safe to retry after a leadership
/// change, which is the only reason the bootstrap loop can simply propose again
/// rather than having to discover whether an earlier attempt survived.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exact_genesis_replay_is_idempotent() {
    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x01, 1);
    let voter = start(&fixture).await;
    let coordination = voter.coordination();

    let before = coordination.effective_voters_for_testing();

    let replay = coordination
        .propose_for_testing_within(
            CommandV1::Genesis(GenesisCommandV1 {
                signed_genesis: fixture.signed_genesis(),
            }),
            PATIENCE,
        )
        .await
        .expect("an exact replay commits");
    assert_eq!(
        system_code(&replay),
        SYS_GENESIS_ALREADY_INSTALLED,
        "a second arrival of the same document is recognised, not reinstalled"
    );

    // And the group is unchanged and still working.
    assert_eq!(coordination.effective_voters_for_testing(), before);
    assert_eq!(coordination.lifecycle_code(), 3, "still Ready");

    voter.server.shutdown().await;
}

/// **ROOT cannot be made, or unmade, a delegated administrator.**
///
/// Both directions, because immutable means immutable in both. Enabling ROOT
/// would put it in the mutable table, where a later `set_admin` could remove it
/// and the final-administrator rule would start counting it — so a group could
/// "remove ROOT" and reach a state where the key that founded it no longer
/// authorizes anything.
///
/// `26 RootAuthorityImmutable` rather than `12 PermissionChanged`: the caller
/// here holds every authority there is, and what is refused is the *target*.
/// Answering `PermissionChanged` would send an operator to check credentials
/// for a request no credential can make.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_cannot_be_added_to_or_removed_from_the_delegated_set() {
    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x02, 1);
    let voter = start(&fixture).await;
    let coordination = voter.coordination();

    for (enabled, what) in [(true, "enable"), (false, "disable")] {
        let committed = coordination
            .propose_for_testing_within(
                client(
                    &fixture,
                    0,
                    &format!("root-{what}"),
                    ActionV1::SetAdmin(SetAdminActionV1 {
                        target_node_id: fixture.authority_key().to_vec(),
                        enabled,
                        expected_admin_revision: 1,
                    }),
                ),
                PATIENCE,
            )
            .await
            .expect("the command commits");
        assert_eq!(
            outcome_of(&committed),
            (REJECTED, ROOT_AUTHORITY_IMMUTABLE),
            "set_admin must not {what} ROOT"
        );
    }

    voter.server.shutdown().await;
}

/// **A delegate can be added, and the final one cannot be removed — terminally.**
///
/// The §8.1 rule, and the correction that made it terminal: the escape ("add a
/// replacement, then retry") advances `admin_revision`, so the original
/// revision-fenced removal can only fail `AdminRevisionMismatch` afterwards.
/// The condition clears; that request never applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_delegated_set_is_mutable_but_never_empty() {
    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x03, 1);
    let voter = start(&fixture).await;
    let coordination = voter.coordination();

    // The genesis named exactly one delegate, so removing it empties the set.
    let removal = client(
        &fixture,
        0,
        "remove-last",
        ActionV1::SetAdmin(SetAdminActionV1 {
            target_node_id: fixture.node_id(0),
            enabled: false,
            expected_admin_revision: 1,
        }),
    );
    let committed = coordination
        .propose_for_testing_within(removal.clone(), PATIENCE)
        .await
        .expect("the removal commits");
    let (outcome, reason) = outcome_of(&committed);
    assert_eq!(outcome, REJECTED, "the final delegate cannot be removed");
    assert_eq!(reason, 24, "§8.1's LastAdministrator");

    // Add a second delegate: legal, and it advances the revision.
    let other = [0xEEu8; 32];
    let committed = coordination
        .propose_for_testing_within(
            client(
                &fixture,
                0,
                "add-second",
                ActionV1::SetAdmin(SetAdminActionV1 {
                    target_node_id: other.to_vec(),
                    enabled: true,
                    expected_admin_revision: 1,
                }),
            ),
            PATIENCE,
        )
        .await
        .expect("the addition commits");
    assert_eq!(outcome_of(&committed), (ADMIN_UPDATED, NO_REASON));

    // The *unchanged* removal now fails on the revision it was fenced against,
    // which is why `LastAdministrator` is terminal for the request that got it.
    // A different client id, because a client may not alter a sequence it has
    // already sent.
    let refenced = client(
        &fixture,
        0,
        "remove-last-again",
        ActionV1::SetAdmin(SetAdminActionV1 {
            target_node_id: fixture.node_id(0),
            enabled: false,
            expected_admin_revision: 1,
        }),
    );
    let committed = coordination
        .propose_for_testing_within(refenced, PATIENCE)
        .await
        .expect("the retry commits");
    assert_eq!(
        outcome_of(&committed),
        (REJECTED, ADMIN_REVISION_MISMATCH),
        "the replacement advanced the revision the original request named"
    );

    voter.server.shutdown().await;
}

/// **ROOT acts as an administrator across every administrative surface.**
///
/// One predicate, four surfaces, and the reason to test all four rather than
/// one: `retire_client` read `admins` directly until this was wired up, so ROOT
/// could change membership and issue grants but not retire a client. A group
/// with one administrative *authority* and a different answer per command is
/// the failure this covers.
///
/// ROOT is not in `initial_admins` — a genesis naming it there does not verify
/// — so every acceptance here is the ROOT branch of the predicate and nothing
/// else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_is_an_administrator_on_every_administrative_path() {
    use aster::coordination::contract::{
        CancelMembershipActionV1, ChangeMembershipActionV1, NodeIdV1, RetireClientCommandV1,
    };

    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x04, 1);
    let voter = start(&fixture).await;
    let coordination = voter.coordination();
    let root = fixture.authority_key().to_vec();

    assert!(
        !coordination
            .effective_nodes_for_testing()
            .iter()
            .any(|node| node.as_bytes() == root.as_slice()),
        "ROOT is never a committed member"
    );

    // 1. A grant, as ROOT.
    let action = ActionV1::SetGrant(SetGrantActionV1 {
        resource: ResourceKeyV1 {
            namespace: "root-gate".into(),
            key: b"grant".to_vec(),
        },
        target_node_id: fixture.node_id(0),
        rights: aster::coordination::model::limits::RIGHT_READ,
        expected_acl_revision: 0,
    });
    let committed = coordination
        .propose_for_testing_within(
            CommandV1::Client(ClientCommandV1 {
                actor_node_id: root.clone(),
                client_id: aster::coordination::testing::id16(b"root-grant"),
                sequence: 1,
                request_hash: aster::coordination::hash::request_hash(&action)
                    .expect("a canonical action hashes")
                    .to_vec(),
                action,
            }),
            PATIENCE,
        )
        .await
        .expect("commits");
    assert_eq!(
        outcome_of(&committed),
        (GRANT_UPDATED, NO_REASON),
        "ROOT may grant"
    );

    // 2. `set_admin`, as ROOT, on a node that is not ROOT.
    let action = ActionV1::SetAdmin(SetAdminActionV1 {
        target_node_id: [0xC1u8; 32].to_vec(),
        enabled: true,
        expected_admin_revision: 1,
    });
    let committed = coordination
        .propose_for_testing_within(
            CommandV1::Client(ClientCommandV1 {
                actor_node_id: root.clone(),
                client_id: aster::coordination::testing::id16(b"root-admin"),
                sequence: 1,
                request_hash: aster::coordination::hash::request_hash(&action)
                    .expect("a canonical action hashes")
                    .to_vec(),
                action,
            }),
            PATIENCE,
        )
        .await
        .expect("commits");
    assert_eq!(
        outcome_of(&committed),
        (ADMIN_UPDATED, NO_REASON),
        "ROOT may appoint a delegate"
    );

    // 3. A membership change, as ROOT. Refused on its merits here — a one-voter
    //    group has no other legal shape — but the *reason* is what matters: it
    //    must not be `PermissionChanged`, which is what a ROOT that failed the
    //    predicate would receive.
    let action = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
        operation_id: aster::coordination::testing::id16(b"root-membership"),
        expected_membership_log_id: coordination.membership_log_id_for_testing(),
        desired_voters: vec![NodeIdV1 {
            node_id: fixture.node_id(0),
        }],
        retained_learners: Vec::new(),
        member_seeds: Vec::new(),
    });
    let committed = coordination
        .propose_for_testing_within(
            CommandV1::Client(ClientCommandV1 {
                actor_node_id: root.clone(),
                client_id: aster::coordination::testing::id16(b"root-membership"),
                sequence: 1,
                request_hash: aster::coordination::hash::request_hash(&action)
                    .expect("a canonical action hashes")
                    .to_vec(),
                action,
            }),
            PATIENCE,
        )
        .await
        .expect("commits");
    assert_ne!(
        outcome_of(&committed).1,
        PERMISSION_CHANGED,
        "ROOT must pass the administrative predicate for membership"
    );

    // 4. Cancellation, same rule.
    let action = ActionV1::CancelMembership(CancelMembershipActionV1 {
        operation_id: aster::coordination::testing::id16(b"root-membership"),
        expected_phase_code: 1,
    });
    let committed = coordination
        .propose_for_testing_within(
            CommandV1::Client(ClientCommandV1 {
                actor_node_id: root.clone(),
                client_id: aster::coordination::testing::id16(b"root-cancel"),
                sequence: 1,
                request_hash: aster::coordination::hash::request_hash(&action)
                    .expect("a canonical action hashes")
                    .to_vec(),
                action,
            }),
            PATIENCE,
        )
        .await
        .expect("commits");
    assert_ne!(
        outcome_of(&committed).1,
        PERMISSION_CHANGED,
        "ROOT must pass the administrative predicate for cancellation"
    );

    // 5. Retiring **another actor's** session — the surface that read `admins`
    //    directly and would have refused ROOT.
    let committed = coordination
        .propose_for_testing_within(
            CommandV1::RetireClient(RetireClientCommandV1 {
                actor_node_id: root,
                target_node_id: fixture.node_id(0),
                target_client_id: aster::coordination::testing::id16(b"someone-else"),
            }),
            PATIENCE,
        )
        .await
        .expect("commits");
    match &committed.reply {
        CommandReplyV1::Retirement(reply) => assert_ne!(
            reply.reason_code, PERMISSION_CHANGED,
            "ROOT may retire another actor's session"
        ),
        other => panic!("a retirement answers with a Retirement reply, got {other:?}"),
    }

    voter.server.shutdown().await;
}

/// **An exact replay stays idempotent after the state has moved on.**
///
/// The check that compares the genesis's `initial_voters` with the committed
/// membership belongs to *first installation* only. Running it on every arrival
/// made idempotence expire: once the group legally changed its membership or
/// its administrators, the same document the group was founded on would find a
/// set that no longer matched and be answered `GenesisRefused` — fatal, on
/// every voter, for the one document they all agree on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replay_survives_the_state_moving_on() {
    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x05, 1);
    let voter = start(&fixture).await;
    let coordination = voter.coordination();

    // Move the administrator state on, so a replay is no longer arriving into
    // the state the genesis installed.
    let action = ActionV1::SetAdmin(SetAdminActionV1 {
        target_node_id: [0xD4u8; 32].to_vec(),
        enabled: true,
        expected_admin_revision: 1,
    });
    let committed = coordination
        .propose_for_testing_within(
            CommandV1::Client(ClientCommandV1 {
                actor_node_id: fixture.node_id(0),
                client_id: aster::coordination::testing::id16(b"evolve"),
                sequence: 1,
                request_hash: aster::coordination::hash::request_hash(&action)
                    .expect("a canonical action hashes")
                    .to_vec(),
                action,
            }),
            PATIENCE,
        )
        .await
        .expect("commits");
    assert_eq!(outcome_of(&committed), (ADMIN_UPDATED, NO_REASON));

    let replay = coordination
        .propose_for_testing_within(
            CommandV1::Genesis(GenesisCommandV1 {
                signed_genesis: fixture.signed_genesis(),
            }),
            PATIENCE,
        )
        .await
        .expect("an exact replay commits after the state has evolved");
    assert_eq!(system_code(&replay), SYS_GENESIS_ALREADY_INSTALLED);

    // Unchanged and still serving: the replay is a business-state no-op, not a
    // reinstallation that would have reset the delegated set to the genesis's.
    assert_eq!(coordination.lifecycle_code(), 3, "still Ready");
    let after = coordination
        .propose_for_testing_within(
            CommandV1::Client(ClientCommandV1 {
                actor_node_id: fixture.node_id(0),
                client_id: aster::coordination::testing::id16(b"after-replay"),
                sequence: 1,
                request_hash: aster::coordination::hash::request_hash(&ActionV1::SetAdmin(
                    SetAdminActionV1 {
                        target_node_id: [0xD5u8; 32].to_vec(),
                        enabled: true,
                        expected_admin_revision: 2,
                    },
                ))
                .expect("a canonical action hashes")
                .to_vec(),
                action: ActionV1::SetAdmin(SetAdminActionV1 {
                    target_node_id: [0xD5u8; 32].to_vec(),
                    enabled: true,
                    expected_admin_revision: 2,
                }),
            }),
            PATIENCE,
        )
        .await
        .expect("commits");
    assert_eq!(
        outcome_of(&after),
        (ADMIN_UPDATED, NO_REASON),
        "the revision the earlier change reached survived the replay"
    );

    voter.server.shutdown().await;
}

/// **A different genesis is fatal, and fatal before anything is written.**
///
/// Two genesis documents for one group is a disagreement no voter can resolve
/// locally, so the entry stops this node rather than installing an
/// administrator set the operator never signed for it. `Fault::GenesisRefused`
/// is raised before `last_applied` moves, on the same rule as `UnknownCommand`:
/// a voter that cannot interpret a committed entry must not record having
/// applied it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_different_genesis_is_fatal_and_writes_nothing() {
    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x06, 1);
    let voter = start(&fixture).await;
    let coordination = voter.coordination();

    let before = coordination
        .last_applied_for_testing()
        .expect("an applied index");

    // A different group's genesis, signed by a different ROOT — so it does not
    // even verify under ours, which is the first of the checks.
    let other = GroupFixture::new(DOMAIN, SEED ^ 0x07, 1);
    let outcome = coordination
        .propose_for_testing_within(
            CommandV1::Genesis(GenesisCommandV1 {
                signed_genesis: other.signed_genesis(),
            }),
            PATIENCE,
        )
        .await;
    assert!(
        outcome.is_err(),
        "a foreign genesis must not be accepted: {outcome:?}"
    );

    // Nothing was written for it. The entry is committed — apply cannot skip a
    // committed entry — but the *state machine* refused it before `last_applied`
    // moved, so the node stops rather than recording it as applied.
    let after = coordination.last_applied_for_testing();
    assert!(
        after <= Some(before),
        "a refused genesis must not advance the applied index: {before} -> {after:?}"
    );

    // And the node reports Fatal rather than continuing to serve.
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if coordination.lifecycle_code() == 6 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a voter that refused a genesis must latch Fatal, reported {}",
            coordination.lifecycle_code()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    voter.server.shutdown().await;
}

/// **A restart through `Existing` can still finish an interrupted bootstrap.**
///
/// The genesis is installed by a committed entry, so there is a window: native
/// membership commits, and the genesis entry has not applied yet. A voter that
/// crashed there comes back at `admin_revision = 0` — no administrators, and no
/// administrator able to appoint one. `Existing` carried no genesis, so a
/// restart could never propose it, and the group would sit at `NoQuorum`
/// forever with nothing to explain why.
///
/// Requiring the document on `Existing` closes it. This gate takes the ordinary
/// route — start, stop, restart through `Existing` — and requires the restarted
/// voter to be `Ready` and able to authorize, which is only true if the genesis
/// is installed. A restart into a group that already has one proposes nothing:
/// the proposal is gated on revision 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_through_existing_reaches_ready_and_can_authorize() {
    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x08, 1);
    let dir = tempfile::tempdir().expect("a storage directory");

    let first = AsterServer::builder()
        .config(fixture.aster_config(0))
        .builtin_coordination(fixture.config(dir.path(), 0))
        .start()
        .await
        .expect("the first start");
    tokio::time::timeout(PATIENCE, first.coordination().expect("hosted").wait_ready())
        .await
        .expect("ready in time")
        .expect("ready");
    first.shutdown().await;

    // Reopen. `Existing` verifies the supplied genesis against the directory's
    // bound hash, so this is the same document by construction.
    let second = AsterServer::builder()
        .config(fixture.aster_config(0))
        .builtin_coordination(fixture.existing_config(dir.path()))
        .start()
        .await
        .expect("the group reopens");
    let coordination = second.coordination().expect("hosted");
    tokio::time::timeout(PATIENCE, coordination.wait_ready())
        .await
        .expect("ready in time")
        .expect("ready");

    assert!(
        coordination.genesis_installed_for_testing().await,
        "a restarted voter reporting Ready has its genesis"
    );
    let committed = coordination
        .propose_for_testing_within(set_grant(&fixture, 0, "after-restart"), PATIENCE)
        .await
        .expect("the grant commits");
    assert_eq!(
        outcome_of(&committed),
        (GRANT_UPDATED, NO_REASON),
        "a restarted group can still authorize"
    );

    second.shutdown().await;
}

/// **ROOT is not a member, and cannot be made one.**
///
/// Frozen deliberately, and this is the reasoning rather than a preference. A
/// voter or learner runs its identity's secret as its Aster endpoint secret on
/// a live, dialable, always-on mesh node. ROOT's secret is the deployment's
/// offline trust anchor. A short-lived administration endpoint holding it is
/// fine — brought online on purpose, does one thing, stops; a *member* is the
/// permanent version of the exposure that keeping ROOT offline exists to avoid.
///
/// Both doors: a genesis naming ROOT as an initial voter does not verify, and a
/// membership plan naming ROOT is shapeless. Closing only the first would leave
/// a group that was founded correctly and then had ROOT added by an ordinary
/// change.
#[test]
fn root_cannot_be_founded_as_a_member() {
    use aster::coordination::model::authority::{
        canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts, verify_genesis,
    };
    use aster::coordination::raft::{GenesisV1, NodeIdV1 as WireNodeId, VoterSeedV1};

    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x09, 1);
    let root = fixture.authority_key().to_vec();

    // A genesis that names ROOT as a voter, otherwise well formed and
    // correctly signed by ROOT itself.
    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id: root.clone().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a canonical ticket");

    let genesis = GenesisV1 {
        format_version: 1,
        group_id: vec![SEED ^ 0x09; 32],
        authority_domain: DOMAIN.into(),
        genesis_nonce: vec![0x01u8; 32],
        deployment_profile_code: 2,
        initial_voters: vec![VoterSeedV1 {
            node_id: root.clone(),
            ticket,
        }],
        initial_admins: vec![WireNodeId {
            node_id: fixture.node_id(0),
        }],
        recovery_export_hash: None,
    };
    let document = canonical_document_bytes(&genesis).expect("encode");
    let signature = fixture.sign_for_testing(&genesis_signing_bytes(&document));
    let signed =
        signed_document_from_parts(document, &fixture.authority_key(), &signature).expect("encode");

    let err = verify_genesis(&fixture.authority_key(), DOMAIN, &signed)
        .expect_err("a genesis naming ROOT as a voter must not verify");
    assert!(
        format!("{err:?}").contains("RootIsNotAMember"),
        "the refusal must name the rule, got {err:?}"
    );
}

/// **An interrupted bootstrap recovers: bound storage, no membership.**
///
/// The real cut, which the restart-after-`Ready` gate does not construct. A
/// fresh open commits both identity bindings *before* `Raft::initialize` runs,
/// so a crash in between leaves a directory that is bound and has no
/// membership. `Bootstrap` then refuses it — the directory is no longer
/// `Fresh` — and if `Existing` also did nothing the group could never elect,
/// never propose its genesis, and never leave `NoQuorum`, with no error to
/// explain it.
///
/// The fixture binds the directory through the production storage path and
/// stops, which is exactly the state a crash there leaves. Restarting through
/// `Existing` must then complete the founding: elect, install the genesis,
/// reach `Ready`, and authorize.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_bootstrap_recovers_through_existing() {
    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x0A, 1);
    let dir = tempfile::tempdir().expect("a storage directory");

    // Bind, and stop before `Raft::initialize`.
    fixture.bind_without_initializing_for_testing(dir.path(), &fixture.node_id(0));

    // `Bootstrap` is no longer available: the directory is bound.
    let refused = AsterServer::builder()
        .config(fixture.aster_config(0))
        .builtin_coordination(fixture.config(dir.path(), 0))
        .start()
        .await;
    assert!(
        refused.is_err(),
        "a bound directory is not a Bootstrap target"
    );

    // `Existing` is, and it must finish what the interrupted start began.
    let server = AsterServer::builder()
        .config(fixture.aster_config(0))
        .builtin_coordination(fixture.existing_config(dir.path()))
        .start()
        .await
        .expect("the interrupted bootstrap reopens");
    let coordination = server.coordination().expect("hosted");
    tokio::time::timeout(PATIENCE, coordination.wait_ready())
        .await
        .expect("a recovered voter reaches Ready in time")
        .expect("ready");

    assert!(
        coordination.genesis_installed_for_testing().await,
        "the recovered voter installed the genesis it was interrupted before"
    );
    let committed = coordination
        .propose_for_testing_within(set_grant(&fixture, 0, "after-recovery"), PATIENCE)
        .await
        .expect("the grant commits");
    assert_eq!(
        outcome_of(&committed),
        (GRANT_UPDATED, NO_REASON),
        "a recovered group can authorize"
    );

    server.shutdown().await;
}

/// The applied index, once it has stopped moving on its own.
///
/// A fresh leader applies its own blank entry and then the genesis, and
/// `wait_ready` is level-triggered — it reports that the node reached `Ready`,
/// not that its log has come to rest. A gate that means "this proposal did not
/// advance the log" has to start from an index that was not going to advance
/// anyway, or it is measuring startup.
async fn settled_applied_index(coordination: &aster::coordination::Coordination) -> u64 {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    let mut last = coordination.last_applied_for_testing();
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let now = coordination.last_applied_for_testing();
        if now == last {
            return now.expect("an applied index");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the applied index never settled: {last:?} then {now:?}"
        );
        last = now;
    }
}

/// **A correctly signed *alternative* genesis for the same group is fatal.**
///
/// The earlier version used another ROOT and another group, so it never reached
/// past the first check — a document that does not verify is refused for the
/// signature, which proves nothing about the "two genesis documents for one
/// group" rule. This one is signed by the *same* ROOT, names the *same* group
/// and domain, and differs only in its nonce, so it verifies cleanly and is
/// refused on the binding.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_alternative_genesis_for_the_same_group_is_fatal() {
    use aster::coordination::model::authority::{
        canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts,
    };
    use aster::coordination::raft::{GenesisV1, NodeIdV1 as WireNodeId, VoterSeedV1};

    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x0B, 1);
    let voter = start(&fixture).await;
    let coordination = voter.coordination();

    // The mark is taken once the log has **stopped moving**, not at the first
    // opportunity. `wait_ready` is level-triggered and says nothing about the
    // entries a fresh leader is still applying — its blank entry and the
    // genesis itself — so an index read straight after it can still advance on
    // its own. The "a refused genesis must not advance the applied index"
    // assertion below then fails for a reason that has nothing to do with the
    // genesis, which is the most likely explanation for the one unreproduced
    // failure of this test under parallel load; nothing captured the panic at
    // the time, so this is a fix for a plausible cause rather than a confirmed
    // one, and the assertions below now say which is which.
    assert!(
        coordination.genesis_installed_for_testing().await,
        "the group's own genesis must be installed before a second one is offered"
    );
    let before = settled_applied_index(&coordination).await;

    // Same ROOT, same group, same domain, same voters — a different nonce, so a
    // different document with a different hash.
    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id: fixture.node_id(0).try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a canonical ticket");
    let genesis = GenesisV1 {
        format_version: 1,
        group_id: vec![SEED ^ 0x0B; 32],
        authority_domain: DOMAIN.into(),
        // The one difference.
        genesis_nonce: vec![0xFEu8; 32],
        deployment_profile_code: 2,
        initial_voters: vec![VoterSeedV1 {
            node_id: fixture.node_id(0),
            ticket,
        }],
        initial_admins: vec![WireNodeId {
            node_id: fixture.node_id(0),
        }],
        recovery_export_hash: None,
    };
    let document = canonical_document_bytes(&genesis).expect("encode");
    let signature = fixture.sign_for_testing(&genesis_signing_bytes(&document));
    let alternative =
        signed_document_from_parts(document, &fixture.authority_key(), &signature).expect("encode");
    assert_ne!(
        alternative,
        fixture.signed_genesis(),
        "the fixture must produce a genuinely different document"
    );

    let outcome = coordination
        .propose_for_testing_within(
            CommandV1::Genesis(GenesisCommandV1 {
                signed_genesis: alternative,
            }),
            PATIENCE,
        )
        .await;
    // **The refusal has to be the genesis binding rule**, named. `is_err()` was
    // what this asserted, and `is_err()` is satisfied by a timeout, a
    // `ForwardToLeader`, or any transport failure — none of which say anything
    // about two genesis documents for one group. `Fault::GenesisRefused` is
    // rendered into the storage error that ends the apply, so the name is on
    // the wire the caller sees.
    let refusal = match outcome {
        Err(err) => err.to_string(),
        Ok(committed) => panic!(
            "a second genesis for this group must not be accepted; it committed at \
             index {} and replied {:?}",
            committed.index, committed.reply
        ),
    };
    assert!(
        refusal.contains("GenesisRefused"),
        "the refusal must be the genesis binding rule and not something else that \
         merely failed. The proposal answered: {refusal}"
    );

    let after = coordination.last_applied_for_testing();
    assert!(
        after <= Some(before),
        "a refused genesis must not advance the applied index: it was {before} \
         before the proposal and {after:?} after"
    );

    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if coordination.lifecycle_code() == 6 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "two genesis documents for one group is fatal; after the refusal \
             ({refusal}) the voter reported lifecycle {} rather than 6, with applied \
             index {:?}",
            coordination.lifecycle_code(),
            coordination.last_applied_for_testing()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    voter.server.shutdown().await;
}

/// **A membership plan naming ROOT is refused.**
///
/// The second door. `verify_genesis` closes the first; without this a group
/// founded correctly could have ROOT added by an ordinary change. Shapeless
/// rather than reason-coded, because it does not depend on replicated state —
/// ROOT is ROOT at every revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_membership_plan_naming_root_is_refused() {
    use aster::coordination::contract::{ChangeMembershipActionV1, NodeIdV1};

    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x0C, 1);
    let voter = start(&fixture).await;
    let coordination = voter.coordination();

    for (label, voters, learners) in [
        ("as a voter", vec![fixture.authority_key().to_vec()], vec![]),
        (
            "as a learner",
            vec![fixture.node_id(0)],
            vec![fixture.authority_key().to_vec()],
        ),
    ] {
        let action = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
            operation_id: aster::coordination::testing::id16(label.as_bytes()),
            expected_membership_log_id: coordination.membership_log_id_for_testing(),
            desired_voters: voters
                .into_iter()
                .map(|node_id| NodeIdV1 { node_id })
                .collect(),
            retained_learners: learners
                .into_iter()
                .map(|node_id| NodeIdV1 { node_id })
                .collect(),
            member_seeds: Vec::new(),
        });
        let committed = coordination
            .propose_for_testing_within(
                CommandV1::Client(ClientCommandV1 {
                    actor_node_id: fixture.node_id(0),
                    client_id: aster::coordination::testing::id16(label.as_bytes()),
                    sequence: 1,
                    request_hash: aster::coordination::hash::request_hash(&action)
                        .expect("a canonical action hashes")
                        .to_vec(),
                    action,
                }),
                PATIENCE,
            )
            .await
            .expect("the plan commits");
        // `(100, 0)` — shapeless. A reason-coded refusal would say the rule
        // depends on replicated state, and it does not.
        assert_eq!(
            outcome_of(&committed),
            (REJECTED, NO_REASON),
            "a plan naming ROOT {label} must be refused as shapeless"
        );
    }

    // And the membership is untouched.
    assert!(
        !coordination
            .effective_nodes_for_testing()
            .iter()
            .any(|node| node.as_bytes() == &fixture.authority_key()),
        "ROOT never enters the committed membership"
    );

    voter.server.shutdown().await;
}

/// **A genesis naming ROOT as an initial administrator does not verify.**
///
/// The other half of §8.1's exclusion, and previously unevidenced: the existing
/// gate placed ROOT among the *voters*, so it exercised `RootIsNotAMember` and
/// removing the delegate check would have kept passing.
///
/// The document is otherwise well formed — ordinary voters, correct signature,
/// this fixture's own ROOT and domain — so the only thing wrong with it is the
/// administrator entry. A delegated ROOT would be a mutable row: `set_admin`
/// could remove it and the final-delegate rule would start counting it, so a
/// group could "remove ROOT" and reach a state where the key that founded it
/// authorizes nothing.
#[test]
fn root_cannot_be_founded_as_a_delegate() {
    use aster::coordination::model::authority::{
        canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts, verify_genesis,
    };
    use aster::coordination::raft::{GenesisV1, NodeIdV1 as WireNodeId, VoterSeedV1};

    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x0D, 1);
    let voter = fixture.node_id(0);
    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id: voter.clone().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a canonical ticket");

    // Ordinary voters; ROOT appears **only** as an administrator.
    let genesis = GenesisV1 {
        format_version: 1,
        group_id: vec![SEED ^ 0x0D; 32],
        authority_domain: DOMAIN.into(),
        genesis_nonce: vec![0x02u8; 32],
        deployment_profile_code: 2,
        initial_voters: vec![VoterSeedV1 {
            node_id: voter,
            ticket,
        }],
        initial_admins: vec![WireNodeId {
            node_id: fixture.authority_key().to_vec(),
        }],
        recovery_export_hash: None,
    };
    let document = canonical_document_bytes(&genesis).expect("encode");
    let signature = fixture.sign_for_testing(&genesis_signing_bytes(&document));
    let signed =
        signed_document_from_parts(document, &fixture.authority_key(), &signature).expect("encode");

    let err = verify_genesis(&fixture.authority_key(), DOMAIN, &signed)
        .expect_err("a genesis naming ROOT as an administrator must not verify");
    assert!(
        format!("{err:?}").contains("RootIsNotDelegable"),
        "the refusal must name the delegate rule, not the member one: {err:?}"
    );
}

/// **A joining node survives restart, in the same restricted state.**
///
/// Its first start binds the directory; every later start opens a bound one.
/// `Join` required `Fresh`, so the second start was refused outright — and
/// deleting the directory is not a workaround, because the group's quorum may
/// already count the candidate a voter, so a wiped store would rejoin as a
/// member with no log.
///
/// §S3-JOIN is explicit that a restart "retains the same restricted state until
/// membership lands". What this gate observes is the **lifecycle** half of that:
/// the same permit is accepted again, the corridor is `Joining` both times, and
/// no membership appears. The candidate stays at `Joining` because no
/// administrator ever commits an intent for it — a permit grants no membership,
/// which is the other half of the same rule.
///
/// It does **not** observe network admission. The permit's sender allowlist is
/// covered where it can be checked directly, by
/// `coordination_cluster::an_unauthorized_identity_never_reaches_openraft` and
/// the `boot` corridor-plan tests, which compare admitted against unadmitted
/// senders. Saying "same restricted allowlist" here would claim evidence this
/// test does not hold.
///
/// The permit is **re-verified** at each start rather than persisted. It binds
/// the candidate's NodeId, so re-verifying costs no new durable fact; a store
/// that remembered a permit would be a second place for the group's opinion of
/// this node to live, and the two could disagree after a revocation.
///
/// Both starts are real: the candidate starts, is shut down, and starts again
/// on the same directory, the same identity and the same permit. Nothing is
/// constructed on its behalf, because "a restart works" is precisely the claim
/// and a hand-built directory cannot support it.
///
/// `AsterConfig` names no `bind_addr`, so each start takes whatever ephemeral
/// port it is given, exactly as a real node does. The gate does not assert the
/// two ports differ — port allocation is not contractual, and an OS that reused
/// one would not make the old address binding correct. What makes this
/// non-vacuous is the permit itself: it carries the **addressless** ticket, so
/// the pre-`S3-JOIN-TICKET` verifier rejects it on the *first* start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joining_node_survives_restart_in_the_same_restricted_state() {
    use aster::coordination::model::authority::canonical_candidate_ticket;

    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x0E, 1);
    let candidate = [0xB9u8; 32];
    let dir = tempfile::tempdir().expect("a storage directory");

    let node_config = || {
        aster::AsterConfig::builder()
            .relay(aster::RelayMode::Disabled)
            .secret_key(aster::SecretKey::from_bytes(candidate))
            .root_pubkey(aster::PublicKey::from_bytes(fixture.authority_key()))
            .build()
    };

    // The permit is minted once, from the candidate's NodeId alone — which is
    // all an operator has and all they need. No node is started to obtain it,
    // and this is production's own derivation rather than a second copy of it.
    let node_id = fixture_node_id(candidate);
    let ticket = canonical_candidate_ticket(&node_id).expect("a canonical ticket");

    // The property that makes the permit restart-survivable, asserted rather
    // than assumed: what it binds carries no address at all.
    let decoded = aster::Ticket::from_base58(&ticket).expect("a well-formed ticket");
    assert!(
        decoded.direct_addresses().is_empty(),
        "a permit's ticket must name no address: {:?}",
        decoded.direct_addresses()
    );

    // First start: a fresh directory, which is what a candidate has the first
    // time. Starting binds it.
    let first = AsterServer::builder()
        .config(node_config())
        .builtin_coordination(fixture.join_config(dir.path(), candidate, &ticket, &[0]))
        .start()
        .await
        .expect("a candidate joins with a valid permit");
    assert_eq!(
        first.coordination().expect("hosted").lifecycle_code(),
        2,
        "a permit grants no membership, so the first start is Joining"
    );
    assert_eq!(
        first.id().to_string(),
        hex::encode(node_id),
        "the permit is for this NodeId, and the node is running under it"
    );
    first.shutdown().await;

    // Second start: same directory — now bound — same identity, same permit.
    // This is the case `Join` refused outright when it required a `Fresh`
    // directory. Wiping instead is not a workaround: the group's quorum may
    // already count this candidate a voter, and a wiped store would rejoin as
    // a member with no log.
    let restarted = AsterServer::builder()
        .config(node_config())
        .builtin_coordination(fixture.join_config(dir.path(), candidate, &ticket, &[0]))
        .start()
        .await
        .expect("a joining candidate restarts into a directory it already bound");

    assert_eq!(
        restarted.coordination().expect("hosted").lifecycle_code(),
        2,
        "the restart retains the same restricted lifecycle state until \
         membership lands"
    );
    assert_eq!(
        restarted.id().to_string(),
        hex::encode(node_id),
        "the same identity came back, which is what makes this a restart"
    );

    restarted.shutdown().await;
}

/// The NodeId a secret produces, without starting anything — all an operator
/// needs to mint a permit, and the only input the permit's ticket derives from.
fn fixture_node_id(secret: [u8; 32]) -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(&secret)
        .verifying_key()
        .to_bytes()
}
