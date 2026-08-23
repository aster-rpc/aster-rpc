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
use aster::{AsterConfig, RelayMode, SecretKey};

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
        .config(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes(fixture.secret(0)))
                .build(),
        )
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

    let before = coordination.committed_voters_for_testing();

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
    assert_eq!(coordination.committed_voters_for_testing(), before);
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
