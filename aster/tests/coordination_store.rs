//! §S4-C: the live `aster.coordination.Store` service, driven over real Iroh
//! RPC by real clients with distinct authenticated identities.
//!
//! ## The actors
//!
//! Every gate runs three kinds of caller against a real one-voter group (the
//! redirect gate runs three voters):
//!
//! * **ROOT** — a client whose *endpoint secret* is the deployment ROOT
//!   signing key. That is the §8.1 break-glass identity: its authenticated
//!   NodeId equals the replicated ROOT public key, and every administrative
//!   decision must recognise it without a delegated row.
//! * **worker** — an ordinary authenticated node that holds exactly the
//!   committed grants ROOT gave it, and nothing else.
//! * **stranger** — an authenticated node with no grants and no role. The
//!   §S4-C exit says an arbitrary admitted consumer gets no
//!   resource-existence, membership, ACL, admin, audit or leader oracle, and
//!   the stranger is who that is proven against.
//!
//! Permissions are asserted per method and per §5.2 permission row, not as
//! "some denied caller somewhere": an administrator has no implicit resource
//! right, a holder has no inspect right, and the same method that refuses the
//! stranger answers its authorized caller in the same run — so a gate that
//! passed because the service refused *everyone* cannot pass.

#![cfg(feature = "coordination-testing")]

use std::time::Duration;

use aster::coordination::contract::{payload_fory, FenceV1, ResourceKeyV1, ResourceSnapshotV1};
use aster::coordination::model::limits::{
    RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE, RIGHT_RENEW, RIGHT_REVOKE,
};

use aster::coordination::service::{STORE_SERVICE, STORE_VERSION};
use aster::coordination::store::wire::{
    AclStatusReplyWireV1, AclStatusRequestV1, AcquireRequestV1, AdminStatusReplyWireV1,
    AdminStatusRequestV1, AuditRecordWireV1, AuditStreamRequestV1, CompareAndSetRequestV1,
    GroupStatusReplyWireV1, GroupStatusRequestV1, MembershipStatusReplyWireV1,
    MembershipStatusRequestV1, MutationReplyWireV1, ProbeClientReplyWireV1, ProbeClientRequestV1,
    ReadReplyWireV1, ReadRequestV1, RecoveryExportFrameV1, RecoveryExportRequestV1,
    ReleaseRequestV1, RenewRequestV1, RetireClientReplyWireV1, RetireClientRequestV1,
    RevokeRequestV1, SetAdminRequestV1, SetGrantRequestV1, DETAIL_LEADER_NODE_ID,
    STORE_PROTOCOL_VERSION,
};
use aster::coordination::testing::{id16, GroupFixture};
use aster::rpc::{AsterServer, RpcConnection, StreamHeader};
use aster::{AsterConfig, Error, Node, PublicKey, RelayMode, SecretKey};

/// Give every await in this file a bound, so a wedged pipeline is a failure
/// with a location rather than a hung suite.
const PATIENCE: Duration = Duration::from_secs(60);

async fn within<T, F: std::future::Future<Output = T>>(what: &str, f: F) -> T {
    match tokio::time::timeout(PATIENCE, f).await {
        Ok(value) => value,
        Err(_) => panic!("{what}: no answer within {PATIENCE:?}"),
    }
}

/// One voter, its fixture, and three connected clients.
struct StoreHarness {
    _voter: AsterServer,
    fixture: GroupFixture,
    root: Client,
    worker: Client,
    stranger: Client,
    _dir: tempfile::TempDir,
}

struct Client {
    /// Held so the endpoint stays alive for the connection's lifetime.
    _node: Node,
    conn: RpcConnection,
    id: [u8; 32],
}

impl Client {
    async fn connect(secret: [u8; 32], fixture: &GroupFixture, voter: &AsterServer) -> Self {
        let node = Node::start(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes(secret))
                .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
                .build(),
        )
        .await
        .expect("a client node starts");
        node.add_node_addr(voter.node().addr())
            .expect("register the voter's address");
        let conn = within("client connect", node.rpc_connect(&voter.node().id()))
            .await
            .expect("the client dials the voter");
        let id = ed25519_dalek::SigningKey::from_bytes(&secret)
            .verifying_key()
            .to_bytes();
        Self {
            _node: node,
            conn,
            id,
        }
    }
}

/// The fixture's deterministic ROOT signing secret.
///
/// `GroupFixture` derives its authority key from the seed, so a gate can start
/// a client whose **endpoint identity is ROOT** — the §8.1 break-glass path —
/// without the fixture exporting a secret accessor.
fn root_secret(seed: u8) -> [u8; 32] {
    [seed ^ 0xFF; 32]
}

async fn harness(seed: u8) -> StoreHarness {
    let fixture = GroupFixture::new("store.test", seed, 1);
    let dir = tempfile::tempdir().expect("a storage directory");
    let voter = AsterServer::builder()
        .config(fixture.aster_config(0))
        .builtin_coordination(fixture.config(dir.path(), 0))
        .start()
        .await
        .expect("the voter starts");
    within(
        "voter readiness",
        voter.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("the one-voter group becomes ready");

    let root = Client::connect(root_secret(seed), &fixture, &voter).await;
    let worker = Client::connect([seed ^ 0x41; 32], &fixture, &voter).await;
    let stranger = Client::connect([seed ^ 0x42; 32], &fixture, &voter).await;
    StoreHarness {
        _voter: voter,
        fixture,
        root,
        worker,
        stranger,
        _dir: dir,
    }
}

fn header(method: &str) -> StreamHeader {
    StreamHeader {
        service: STORE_SERVICE.to_string(),
        method: method.to_string(),
        version: STORE_VERSION,
        ..Default::default()
    }
}

fn encode<T>(value: &T) -> Vec<u8>
where
    T: aster::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
{
    payload_fory::<T>().serialize(value).expect("encode")
}

fn decode<T>(bytes: &[u8]) -> T
where
    T: aster::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
{
    payload_fory::<T>().deserialize(bytes).expect("decode")
}

/// Call and expect a typed reply.
async fn call<Req, Reply>(client: &Client, method: &str, request: &Req) -> Reply
where
    Req: aster::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
    Reply:
        aster::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
{
    let raw = within(method, client.conn.unary(&header(method), encode(request)))
        .await
        .unwrap_or_else(|err| panic!("{method}: expected a typed reply, got {err}"));
    decode(&raw)
}

/// Call and expect a non-OK status; return `(code, message, details)`.
async fn call_refused<Req>(
    client: &Client,
    method: &str,
    request: &Req,
) -> (i32, String, Vec<(String, String)>)
where
    Req: aster::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
{
    let err = within(method, client.conn.unary(&header(method), encode(request)))
        .await
        .err()
        .unwrap_or_else(|| panic!("{method}: expected a refusal, got a reply"));
    match err {
        Error::Rpc {
            code,
            message,
            details,
        } => (code, message, details),
        other => panic!("{method}: expected an RPC status, got {other}"),
    }
}

/// Open a server stream, collect every frame, and require a clean end.
async fn stream<Req>(client: &Client, method: &str, request: &Req) -> Vec<Vec<u8>>
where
    Req: aster::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
{
    let mut response = within(
        method,
        client.conn.server_stream(&header(method), encode(request)),
    )
    .await
    .unwrap_or_else(|err| panic!("{method}: the stream opens, got {err}"));
    within(method, response.collect())
        .await
        .unwrap_or_else(|err| panic!("{method}: the stream completes, got {err}"))
}

const PERMISSION_DENIED: i32 = 7;
const INVALID_ARGUMENT: i32 = 3;
const UNAVAILABLE: i32 = 14;

fn resource(key: &[u8]) -> ResourceKeyV1 {
    ResourceKeyV1 {
        namespace: "store.gate".into(),
        key: key.to_vec(),
    }
}

fn group_of(fixture: &GroupFixture) -> Vec<u8> {
    fixture.group_id().as_bytes().to_vec()
}

/// A grant from ROOT, expected committed.
async fn grant(
    harness: &StoreHarness,
    sequence: u64,
    key: &[u8],
    subject: [u8; 32],
    rights: u64,
    expected_acl_revision: u64,
) -> MutationReplyWireV1 {
    let reply: MutationReplyWireV1 = call(
        &harness.root,
        "set_grant",
        &SetGrantRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group_of(&harness.fixture),
            client_id: id16(b"root-admin"),
            sequence,
            resource: resource(key),
            target_node_id: subject.to_vec(),
            rights,
            expected_acl_revision,
        },
    )
    .await;
    assert_eq!(
        (
            reply.disposition_code,
            reply.outcome_code,
            reply.reason_code
        ),
        (1, 6, 0),
        "ROOT's grant commits: {reply:?}"
    );
    reply
}

fn fence_of(snapshot: &ResourceSnapshotV1) -> FenceV1 {
    FenceV1 {
        group_id: snapshot.group_id.clone(),
        resource: snapshot.resource.clone(),
        epoch: snapshot.epoch,
        holder: snapshot.holder.clone().expect("held"),
    }
}

// ── The ACL/admin path first (the §S4 ordering), then resources ─────────────

/// **ROOT administers without a delegated row; the ACL it writes is exact；
/// and the register lifecycle runs end to end on committed grants.**
///
/// One flowing gate rather than many, because the §5.2 lifecycle is a
/// sequence: grant → acquire → renew → compare_and_set → read → release →
/// revoke, each from the actor its permission row names, each answered from
/// the applied state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_admin_path_grants_and_the_register_lifecycle_runs_on_those_grants() {
    let h = harness(0xD1).await;
    let group = group_of(&h.fixture);
    let key = b"head";

    // ROOT grants the worker the full working set on one resource.
    grant(
        &h,
        1,
        key,
        h.worker.id,
        RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
        0,
    )
    .await;

    // The worker acquires...
    let acquired: MutationReplyWireV1 = call(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"worker"),
            sequence: 1,
            resource: resource(key),
            ttl_ms: 30_000,
        },
    )
    .await;
    assert_eq!(
        (acquired.disposition_code, acquired.outcome_code),
        (1, 1),
        "Granted: {acquired:?}"
    );
    let snapshot = acquired.snapshot.expect("an acquire returns the snapshot");
    assert_eq!(snapshot.holder.as_deref(), Some(h.worker.id.as_slice()));
    assert!(snapshot.held);
    let fence = fence_of(&snapshot);

    // ...renews under its fence...
    let renewed: MutationReplyWireV1 = call(
        &h.worker,
        "renew",
        &RenewRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"worker"),
            sequence: 2,
            fence: fence.clone(),
        },
    )
    .await;
    assert_eq!((renewed.disposition_code, renewed.outcome_code), (1, 2));

    // ...mutates the value at the expected revision...
    let mutated: MutationReplyWireV1 = call(
        &h.worker,
        "compare_and_set",
        &CompareAndSetRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"worker"),
            sequence: 3,
            fence: fence.clone(),
            expected_value_revision: snapshot.value_revision,
            new_value: b"v1-head".to_vec(),
        },
    )
    .await;
    assert_eq!((mutated.disposition_code, mutated.outcome_code), (1, 3));
    let mutated_snapshot = mutated.snapshot.expect("a snapshot");
    assert_eq!(mutated_snapshot.value.as_deref(), Some(b"v1-head".as_ref()));
    assert!(!mutated_snapshot.value_redacted);

    // ...reads it back linearizably...
    let read: ReadReplyWireV1 = call(
        &h.worker,
        "read_linearizable",
        &ReadRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            resource: resource(key),
        },
    )
    .await;
    assert_eq!(read.snapshot.value.as_deref(), Some(b"v1-head".as_ref()));
    assert_eq!(
        read.snapshot.value_revision,
        mutated_snapshot.value_revision
    );

    // ...and releases.
    let released: MutationReplyWireV1 = call(
        &h.worker,
        "release",
        &ReleaseRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"worker"),
            sequence: 4,
            fence,
        },
    )
    .await;
    assert_eq!((released.disposition_code, released.outcome_code), (1, 4));
    // A release preserves the value (§10: it is not a way out of budget
    // exhaustion precisely because of this).
    let released_snapshot = released.snapshot.expect("a snapshot");
    assert!(!released_snapshot.held);

    // ROOT revokes with an exact epoch expectation that is stale — nothing
    // changes and the reply says which fence it lost to.
    grant(&h, 2, key, h.root.id, RIGHT_REVOKE, 1).await;
    let stale_revoke: MutationReplyWireV1 = call(
        &h.root,
        "revoke",
        &RevokeRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"root-admin"),
            sequence: 3,
            resource: resource(key),
            expected_epoch: Some(0),
        },
    )
    .await;
    assert_eq!(
        (
            stale_revoke.disposition_code,
            stale_revoke.outcome_code,
            stale_revoke.reason_code
        ),
        (1, 100, 4),
        "a stale conditional revoke is a committed EpochMismatch: {stale_revoke:?}"
    );

    // The unconditional revoke fences whatever is current.
    let revoked: MutationReplyWireV1 = call(
        &h.root,
        "revoke",
        &RevokeRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"root-admin"),
            sequence: 4,
            resource: resource(key),
            expected_epoch: None,
        },
    )
    .await;
    assert_eq!((revoked.disposition_code, revoked.outcome_code), (1, 5));

    // The ACL the group holds is exactly what was granted, and only an
    // administrator may look.
    let acl: AclStatusReplyWireV1 = call(
        &h.root,
        "inspect_acl",
        &AclStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            resource: resource(key),
        },
    )
    .await;
    assert_eq!(acl.acl_snapshot.grants.len(), 2, "worker + ROOT rows");
    assert!(acl
        .acl_snapshot
        .grants
        .iter()
        .any(|g| g.subject_node_id == h.worker.id.to_vec()
            && g.rights
                == (RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE)));
}

/// **Every §5.2 permission row refuses the caller it names, independently.**
///
/// The §S4-C matrix: an administrator has no implicit resource right; a
/// grant-holder has no admin right; a stranger has neither; and the resource
/// rights are per-bit — holding five of the six does not confer the sixth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_permission_row_is_enforced_independently() {
    let h = harness(0xD2).await;
    let group = group_of(&h.fixture);
    let key = b"guarded";

    // The worker gets READ only.
    grant(&h, 1, key, h.worker.id, RIGHT_READ, 0).await;

    // A stranger gets nothing, and the *pre-proposal* answer for a mutation
    // with no grant is a bare PERMISSION_DENIED status — no sequence consumed.
    let (code, _, _) = call_refused(
        &h.stranger,
        "acquire",
        &AcquireRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"stranger"),
            sequence: 1,
            resource: resource(key),
            ttl_ms: 30_000,
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    // READ does not confer ACQUIRE: the same denial for the worker.
    let (code, _, _) = call_refused(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"worker"),
            sequence: 1,
            resource: resource(key),
            ttl_ms: 30_000,
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED, "READ alone must not acquire");

    // And ROOT — the group's *administrator* — has no implicit resource
    // right either: administrative authority is not a grant.
    let (code, _, _) = call_refused(
        &h.root,
        "read_linearizable",
        &ReadRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            resource: resource(key),
        },
    )
    .await;
    assert_eq!(
        code, PERMISSION_DENIED,
        "an administrator cannot read a register"
    );

    // The worker CAN read — that is its one grant...
    let _: ReadReplyWireV1 = call(
        &h.worker,
        "read_linearizable",
        &ReadRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            resource: resource(key),
        },
    )
    .await;

    // ...and nothing administrative: every administrator-gated method refuses
    // it with the same status, and answers ROOT in the same run.
    let (code, _, _) = call_refused(
        &h.worker,
        "inspect_acl",
        &AclStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            resource: resource(key),
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    let (code, _, _) = call_refused(
        &h.worker,
        "inspect_admins",
        &AdminStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    let (code, _, _) = call_refused(
        &h.worker,
        "membership_status",
        &MembershipStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            operation_id: None,
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    let (code, _, _) = call_refused(
        &h.worker,
        "group_status",
        &GroupStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    let (code, _, _) = call_refused(
        &h.worker,
        "audit_stream",
        &AuditStreamRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            after_log_id: None,
            limit: 100,
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    let (code, _, _) = call_refused(
        &h.worker,
        "recovery_export",
        &RecoveryExportRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    // set_grant / set_admin from a non-administrator: same rule. Sequence 1,
    // because §8.2.1 resolves the session *first* — a gapped sequence would be
    // answered `SequenceGap` at stage 2 before authorization is ever asked,
    // which is exactly the ordering the stranding argument requires.
    let (code, _, _) = call_refused(
        &h.worker,
        "set_grant",
        &SetGrantRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"worker-admin-try"),
            sequence: 1,
            resource: resource(key),
            target_node_id: h.worker.id.to_vec(),
            rights: RIGHT_READ,
            expected_acl_revision: 1,
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    let (code, _, _) = call_refused(
        &h.worker,
        "set_admin",
        &SetAdminRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"worker-admin-try"),
            sequence: 1,
            target_node_id: h.worker.id.to_vec(),
            enabled: true,
            expected_admin_revision: 1,
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    // And the ordering itself, pinned: the same denied caller at a *gapped*
    // sequence gets the classifier's typed answer, not the permission status —
    // stage 2 precedes stage 4 even for a request that would be denied.
    let gapped: MutationReplyWireV1 = call(
        &h.worker,
        "set_admin",
        &SetAdminRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"worker-admin-try"),
            sequence: 5,
            target_node_id: h.worker.id.to_vec(),
            enabled: true,
            expected_admin_revision: 1,
        },
    )
    .await;
    assert_eq!(
        (
            gapped.disposition_code,
            gapped.outcome_code,
            gapped.reason_code
        ),
        (2, 100, 9),
        "session resolution precedes the authorization denial: {gapped:?}"
    );

    // probe_client: an actor may probe itself, not another.
    let _: ProbeClientReplyWireV1 = call(
        &h.stranger,
        "probe_client",
        &ProbeClientRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            target_node_id: h.stranger.id.to_vec(),
            target_client_id: id16(b"stranger"),
        },
    )
    .await;
    let (code, _, _) = call_refused(
        &h.stranger,
        "probe_client",
        &ProbeClientRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            target_node_id: h.worker.id.to_vec(),
            target_client_id: id16(b"worker"),
        },
    )
    .await;
    assert_eq!(
        code, PERMISSION_DENIED,
        "a stranger cannot probe another actor"
    );

    // retire_client: another actor's session is refused; an administrator may.
    let (code, _, _) = call_refused(
        &h.stranger,
        "retire_client",
        &RetireClientRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            target_node_id: h.worker.id.to_vec(),
            target_client_id: id16(b"worker"),
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);
}

/// **The stranger sees no oracle.** Every refusal a stranger receives is the
/// same status whether or not the thing it asked about exists, so existence
/// cannot be inferred from the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_does_not_depend_on_resource_existence() {
    let h = harness(0xD3).await;
    let group = group_of(&h.fixture);

    // One resource exists (ROOT grants the worker on it); one never will.
    grant(&h, 1, b"real", h.worker.id, RIGHT_READ, 0).await;

    let refusal_of = |key: &'static [u8]| {
        let group = group.clone();
        let stranger = &h.stranger;
        async move {
            call_refused(
                stranger,
                "read_linearizable",
                &ReadRequestV1 {
                    protocol_version: STORE_PROTOCOL_VERSION,
                    group_id: group,
                    resource: resource(key),
                },
            )
            .await
        }
    };
    let (real_code, real_msg, _) = refusal_of(b"real").await;
    let (ghost_code, ghost_msg, _) = refusal_of(b"ghost").await;
    assert_eq!(real_code, PERMISSION_DENIED);
    assert_eq!(
        (real_code, real_msg),
        (ghost_code, ghost_msg),
        "the refusal must not distinguish an existing resource from a missing one"
    );
}

/// **The session discipline over the wire**: exact replay answers the saved
/// result redacted, a changed request at a sent sequence is a conflict, a
/// skipped sequence is a gap — and none of them disturb the register.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_classifier_answers_replays_conflicts_and_gaps() {
    let h = harness(0xD4).await;
    let group = group_of(&h.fixture);
    let key = b"seq";
    grant(
        &h,
        1,
        key,
        h.worker.id,
        RIGHT_READ | RIGHT_ACQUIRE | RIGHT_MUTATE | RIGHT_RENEW | RIGHT_RELEASE,
        0,
    )
    .await;

    let acquire = AcquireRequestV1 {
        protocol_version: STORE_PROTOCOL_VERSION,
        group_id: group.clone(),
        client_id: id16(b"worker-seq"),
        sequence: 1,
        resource: resource(key),
        ttl_ms: 30_000,
    };
    let first: MutationReplyWireV1 = call(&h.worker, "acquire", &acquire).await;
    assert_eq!((first.disposition_code, first.outcome_code), (1, 1));
    let first_snapshot = first.snapshot.clone().expect("a snapshot");
    assert!(!first_snapshot.value_redacted);

    // Exact replay: the saved result, value redacted, everything else equal.
    let replayed: MutationReplyWireV1 = call(&h.worker, "acquire", &acquire).await;
    assert_eq!(replayed.disposition_code, 1);
    assert_eq!(replayed.outcome_code, first.outcome_code);
    assert_eq!(replayed.sequence, first.sequence);
    assert_eq!(replayed.request_hash, first.request_hash);
    let replay_snapshot = replayed.snapshot.expect("a snapshot");
    assert!(replay_snapshot.value_redacted, "a replay redacts the value");
    assert_eq!(replay_snapshot.epoch, first_snapshot.epoch);
    assert_eq!(
        replay_snapshot.lease_revision,
        first_snapshot.lease_revision
    );

    // A different action at the same sequence: SequenceConflict, NotConsumed.
    let conflicting: MutationReplyWireV1 = call(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            ttl_ms: 60_000,
            ..acquire.clone()
        },
    )
    .await;
    assert_eq!(
        (
            conflicting.disposition_code,
            conflicting.outcome_code,
            conflicting.reason_code
        ),
        (2, 100, 10),
        "a changed request at a sent sequence is a conflict: {conflicting:?}"
    );

    // Sequence 5 after 1: SequenceGap, NotConsumed.
    let gapped: MutationReplyWireV1 = call(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            sequence: 5,
            ..acquire.clone()
        },
    )
    .await;
    assert_eq!(
        (
            gapped.disposition_code,
            gapped.outcome_code,
            gapped.reason_code
        ),
        (2, 100, 9),
        "a skipped sequence is a gap: {gapped:?}"
    );

    // And a fresh client id starts at 1 or is told it gapped.
    let fresh_gap: MutationReplyWireV1 = call(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            client_id: id16(b"worker-seq-2"),
            sequence: 2,
            ..acquire.clone()
        },
    )
    .await;
    assert_eq!(
        (fresh_gap.disposition_code, fresh_gap.reason_code),
        (2, 9),
        "a first request is always sequence 1: {fresh_gap:?}"
    );
}

/// **Retirement over the wire**: a tombstone refuses new sequences, preserves
/// the committed replay, retiring an absent session pre-creates its tombstone,
/// and the whole flow is available to the actor itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retirement_freezes_a_session_and_still_replays_its_saved_result() {
    let h = harness(0xD5).await;
    let group = group_of(&h.fixture);
    let key = b"ret";
    grant(&h, 1, key, h.worker.id, RIGHT_READ | RIGHT_ACQUIRE, 0).await;

    let acquire = AcquireRequestV1 {
        protocol_version: STORE_PROTOCOL_VERSION,
        group_id: group.clone(),
        client_id: id16(b"to-retire"),
        sequence: 1,
        resource: resource(key),
        ttl_ms: 30_000,
    };
    let committed: MutationReplyWireV1 = call(&h.worker, "acquire", &acquire).await;
    assert_eq!(committed.outcome_code, 1);

    // The worker retires its own session.
    let retired: RetireClientReplyWireV1 = call(
        &h.worker,
        "retire_client",
        &RetireClientRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            target_node_id: h.worker.id.to_vec(),
            target_client_id: id16(b"to-retire"),
        },
    )
    .await;
    assert_eq!((retired.outcome_code, retired.reason_code), (9, 0));

    // Idempotent.
    let again: RetireClientReplyWireV1 = call(
        &h.worker,
        "retire_client",
        &RetireClientRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            target_node_id: h.worker.id.to_vec(),
            target_client_id: id16(b"to-retire"),
        },
    )
    .await;
    assert_eq!(again.outcome_code, 9);

    // Row 2 precedes row 3: the committed sequence still replays its saved
    // result on the tombstone.
    let replay: MutationReplyWireV1 = call(&h.worker, "acquire", &acquire).await;
    assert_eq!((replay.disposition_code, replay.outcome_code), (1, 1));
    assert!(replay.snapshot.expect("a snapshot").value_redacted);

    // Every further sequence answers the terminal ClientRetired.
    let next: MutationReplyWireV1 = call(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            sequence: 2,
            ..acquire.clone()
        },
    )
    .await;
    assert_eq!(
        (next.disposition_code, next.outcome_code, next.reason_code),
        (2, 100, 11),
        "a retired session refuses new sequences terminally: {next:?}"
    );

    // Retiring a session that never existed creates the tombstone that stops
    // its sequence 1 ever applying.
    let absent: RetireClientReplyWireV1 = call(
        &h.worker,
        "retire_client",
        &RetireClientRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            target_node_id: h.worker.id.to_vec(),
            target_client_id: id16(b"never-used"),
        },
    )
    .await;
    assert_eq!(absent.outcome_code, 9);
    let refused: MutationReplyWireV1 = call(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            client_id: id16(b"never-used"),
            sequence: 1,
            ..acquire
        },
    )
    .await;
    assert_eq!(
        (refused.disposition_code, refused.reason_code),
        (2, 11),
        "the pre-created tombstone refuses sequence 1: {refused:?}"
    );

    // probe_client reports the frozen facts — consumed sequence, retirement —
    // and is available to the actor and to an administrator, in that order.
    for probe_as in [&h.worker, &h.root] {
        let probe: ProbeClientReplyWireV1 = call(
            probe_as,
            "probe_client",
            &ProbeClientRequestV1 {
                protocol_version: STORE_PROTOCOL_VERSION,
                group_id: group.clone(),
                target_node_id: h.worker.id.to_vec(),
                target_client_id: id16(b"to-retire"),
            },
        )
        .await;
        assert!(probe.exists);
        assert_eq!(probe.last_sequence, 1);
        assert!(probe.retired);
        assert_eq!(probe.disposition_code, 1, "the saved result is consumed");
    }
}

/// **The admin registry over the wire**: `set_admin` adds and removes
/// delegates against the exact revision, the final delegate is refused with
/// the terminal 24, and the immutable ROOT cannot be targeted at all (26).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_delegated_admin_set_is_mutable_and_root_is_not() {
    let h = harness(0xD6).await;
    let group = group_of(&h.fixture);

    // The genesis seeded one delegate: the voter itself. ROOT appoints the
    // worker as a second.
    let appointed: MutationReplyWireV1 = call(
        &h.root,
        "set_admin",
        &SetAdminRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"root-admin"),
            sequence: 1,
            target_node_id: h.worker.id.to_vec(),
            enabled: true,
            expected_admin_revision: 1,
        },
    )
    .await;
    assert_eq!(
        (appointed.disposition_code, appointed.outcome_code),
        (1, 7),
        "AdminUpdated: {appointed:?}"
    );
    assert_eq!(appointed.admin_revision, 2);

    // The worker — now a delegate — can inspect.
    let admins: AdminStatusReplyWireV1 = call(
        &h.worker,
        "inspect_admins",
        &AdminStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
        },
    )
    .await;
    assert_eq!(admins.admin_snapshot.admin_revision, 2);
    assert_eq!(admins.admin_snapshot.administrators.len(), 2);
    assert!(
        !admins
            .admin_snapshot
            .administrators
            .iter()
            .any(|a| a.node_id == h.root.id.to_vec()),
        "ROOT is never a delegated row"
    );

    // Remove the voter's row, leaving the worker as the last delegate.
    let voter_id = h.fixture.node_id(0);
    let removed: MutationReplyWireV1 = call(
        &h.root,
        "set_admin",
        &SetAdminRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"root-admin"),
            sequence: 2,
            target_node_id: voter_id,
            enabled: false,
            expected_admin_revision: 2,
        },
    )
    .await;
    assert_eq!((removed.disposition_code, removed.outcome_code), (1, 7));

    // The final delegate cannot be removed: terminal 24, NotConsumed.
    let last: MutationReplyWireV1 = call(
        &h.root,
        "set_admin",
        &SetAdminRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"root-admin"),
            sequence: 3,
            target_node_id: h.worker.id.to_vec(),
            enabled: false,
            expected_admin_revision: 3,
        },
    )
    .await;
    assert_eq!(
        (last.disposition_code, last.outcome_code, last.reason_code),
        (2, 100, 24),
        "the final delegate is refused terminally: {last:?}"
    );

    // ROOT can be neither enabled nor disabled: 26, NotConsumed, terminal.
    for enabled in [true, false] {
        let refused: MutationReplyWireV1 = call(
            &h.root,
            "set_admin",
            &SetAdminRequestV1 {
                protocol_version: STORE_PROTOCOL_VERSION,
                group_id: group.clone(),
                client_id: id16(b"root-admin"),
                sequence: 3,
                target_node_id: h.root.id.to_vec(),
                enabled,
                expected_admin_revision: 3,
            },
        )
        .await;
        assert_eq!(
            (
                refused.disposition_code,
                refused.outcome_code,
                refused.reason_code
            ),
            (2, 100, 26),
            "ROOT is immutable: {refused:?}"
        );
    }
}

/// **The status cut and the streams.** `group_status` carries the replicated
/// cut with its own applied witness; `audit_stream` is bounded, ascending,
/// floor-checked; `recovery_export` streams the checksummed canonical body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_audit_and_export_serve_the_pinned_replicated_cut() {
    let h = harness(0xD7).await;
    let group = group_of(&h.fixture);

    // Commit a few entries so the streams have something to carry.
    grant(&h, 1, b"a", h.worker.id, RIGHT_READ, 0).await;
    grant(&h, 2, b"b", h.worker.id, RIGHT_READ | RIGHT_ACQUIRE, 0).await;

    let status: GroupStatusReplyWireV1 = call(
        &h.root,
        "group_status",
        &GroupStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
        },
    )
    .await;
    assert_eq!(
        status.deployment_profile_code, 2,
        "the one-voter dev profile"
    );
    assert_eq!(status.voters.len(), 1);
    assert!(status.learners.is_empty());
    assert!(!status.membership_joint);
    assert!(
        status.applied_log_id.is_some(),
        "the replicated cut carries its own pinned witness"
    );
    assert!(status.membership_log_id.is_some());
    assert_eq!(status.pending_operation_id, None);
    assert_eq!(status.lifecycle_code, 3, "Ready (advisory)");
    assert_eq!(status.quorum_available, Some(true), "a leader has evidence");
    assert_eq!(status.fatal_code, 0);

    // membership_status with no intent: an empty, well-formed reply.
    let membership: MembershipStatusReplyWireV1 = call(
        &h.root,
        "membership_status",
        &MembershipStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            operation_id: None,
        },
    )
    .await;
    assert_eq!(membership.status_code, 0);
    assert_eq!(membership.operation_id, None);
    assert!(membership.observed_membership_log_id.is_some());

    // The audit stream: ascending, bounded, and refusing a pre-floor start.
    let raw = stream(
        &h.root,
        "audit_stream",
        &AuditStreamRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            after_log_id: None,
            limit: 100,
        },
    )
    .await;
    let rows: Vec<AuditRecordWireV1> = raw.iter().map(|frame| decode(frame)).collect();
    assert!(
        rows.len() >= 3,
        "genesis + two grants at least, got {}",
        rows.len()
    );
    assert!(
        rows.windows(2)
            .all(|w| w[0].log_id.index < w[1].log_id.index),
        "audit rows are strictly ascending by index"
    );
    assert!(
        rows.iter().any(|r| r.operation_code == 7),
        "the set_grant mutations appear with their §5.2 method code"
    );
    assert!(
        rows.iter().any(|r| r.operation_code == 102),
        "the committed genesis installation appears as system code 102"
    );

    // A limit of 1 truncates to the earliest row.
    let raw = stream(
        &h.root,
        "audit_stream",
        &AuditStreamRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            after_log_id: None,
            limit: 1,
        },
    )
    .await;
    assert_eq!(raw.len(), 1);
    let first_row: AuditRecordWireV1 = decode(&raw[0]);
    assert_eq!(first_row.log_id.index, rows[0].log_id.index);

    // `after_log_id` starts strictly after the named row.
    let after = rows[0].log_id.clone();
    let raw = stream(
        &h.root,
        "audit_stream",
        &AuditStreamRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            after_log_id: Some(after.clone()),
            limit: 100,
        },
    )
    .await;
    let positioned: Vec<AuditRecordWireV1> = raw.iter().map(|frame| decode(frame)).collect();
    assert!(positioned.iter().all(|r| r.log_id.index > after.index));

    // recovery_export: a manifest whose checksum matches the reassembled body.
    let raw = stream(
        &h.root,
        "recovery_export",
        &RecoveryExportRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
        },
    )
    .await;
    assert!(raw.len() >= 2, "a manifest and at least one chunk");
    let RecoveryExportFrameV1::Manifest(manifest) = decode::<RecoveryExportFrameV1>(&raw[0]) else {
        panic!("the first frame is the manifest");
    };
    assert!(manifest.applied_log_id.is_some());
    let mut body = Vec::new();
    let mut finished = false;
    for frame in &raw[1..] {
        let RecoveryExportFrameV1::Chunk(chunk) = decode::<RecoveryExportFrameV1>(frame) else {
            panic!("every later frame is a chunk");
        };
        assert_eq!(chunk.offset as usize, body.len(), "chunks are contiguous");
        body.extend_from_slice(&chunk.data);
        finished = chunk.final_chunk;
    }
    assert!(finished, "the last chunk is marked final");
    assert_eq!(body.len() as u64, manifest.total_len);
    assert_eq!(
        blake3::hash(&body).as_bytes().to_vec(),
        manifest.blake3_checksum,
        "the manifest checksum covers exactly the streamed body"
    );
}

/// **Stage 0/stage 4 request hygiene**: undecodable bytes, a wrong protocol
/// version, a wrong group and an oversized value are all `INVALID_ARGUMENT`,
/// and none of them consume the sequence they arrived on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_requests_are_refused_without_consuming_the_sequence() {
    let h = harness(0xD8).await;
    let group = group_of(&h.fixture);
    let key = b"hyg";
    grant(
        &h,
        1,
        key,
        h.worker.id,
        RIGHT_READ | RIGHT_ACQUIRE | RIGHT_MUTATE | RIGHT_RENEW | RIGHT_RELEASE,
        0,
    )
    .await;

    // Undecodable bytes.
    let outcome = within(
        "garbage",
        h.worker.conn.unary(&header("acquire"), vec![0xFF; 64]),
    )
    .await;
    let Err(Error::Rpc { code, .. }) = outcome else {
        panic!("garbage must be refused with an RPC status, got {outcome:?}")
    };
    assert_eq!(code, INVALID_ARGUMENT);

    // Wrong protocol version.
    let (code, _, _) = call_refused(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            protocol_version: 9,
            group_id: group.clone(),
            client_id: id16(b"hygiene"),
            sequence: 1,
            resource: resource(key),
            ttl_ms: 30_000,
        },
    )
    .await;
    assert_eq!(code, INVALID_ARGUMENT);

    // Wrong group.
    let (code, _, _) = call_refused(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: vec![0xEE; 32],
            client_id: id16(b"hygiene"),
            sequence: 1,
            resource: resource(key),
            ttl_ms: 30_000,
        },
    )
    .await;
    assert_eq!(code, INVALID_ARGUMENT);

    // A TTL outside §12.1: a decoded, well-identified request refused at
    // stage 4 — after session resolution, so nothing is consumed.
    let (code, _, _) = call_refused(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"hygiene"),
            sequence: 1,
            resource: resource(key),
            ttl_ms: 1,
        },
    )
    .await;
    assert_eq!(code, INVALID_ARGUMENT);

    // The sequence those refusals arrived on is still open: the well-formed
    // sequence 1 commits now.
    let ok: MutationReplyWireV1 = call(
        &h.worker,
        "acquire",
        &AcquireRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            client_id: id16(b"hygiene"),
            sequence: 1,
            resource: resource(key),
            ttl_ms: 30_000,
        },
    )
    .await;
    assert_eq!(
        (ok.disposition_code, ok.outcome_code),
        (1, 1),
        "no refused attempt consumed sequence 1: {ok:?}"
    );

    // An audit request below the floor bound is OUT_OF_RANGE — asked with an
    // absurdly low id once the floor exists. (With no floor set the same call
    // simply streams; the floor case is covered by the storage tests. Here the
    // limit bound is what the wire enforces.)
    let (code, _, _) = call_refused(
        &h.root,
        "audit_stream",
        &AuditStreamRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group.clone(),
            after_log_id: None,
            limit: 0,
        },
    )
    .await;
    assert_eq!(
        code, INVALID_ARGUMENT,
        "limit 0 is out of the declared bounds"
    );
}

/// **A follower redirects with an authenticated leader hint** (§5.4): the
/// stage-1 gate of the pipeline, proven against a real three-voter group.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_follower_answers_unavailable_with_the_leader_hint() {
    let fixture = GroupFixture::new("store.redirect", 0xD9, 3);
    let dirs: Vec<tempfile::TempDir> = (0..3)
        .map(|_| tempfile::tempdir().expect("a directory"))
        .collect();
    let mut voters = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        voters.push(
            AsterServer::builder()
                .config(fixture.aster_config(index))
                .builtin_coordination(fixture.config(dir.path(), index))
                .start()
                .await
                .expect("a voter starts"),
        );
    }
    // Same-host discovery substitution, as every cluster gate does it.
    let addrs: Vec<_> = voters.iter().map(|v| v.node().addr()).collect();
    for voter in &voters {
        for addr in &addrs {
            if addr.node_id != voter.node().addr().node_id {
                voter
                    .node()
                    .add_node_addr(addr.clone())
                    .expect("register a peer");
            }
        }
    }
    for voter in &voters {
        within(
            "readiness",
            voter.coordination().expect("hosted").wait_ready(),
        )
        .await
        .expect("the group becomes ready");
    }

    // Find the leader and a follower.
    let leader_id = voters[0]
        .coordination()
        .expect("hosted")
        .current_leader_for_testing()
        .expect("a ready group has a leader");
    let follower = voters
        .iter()
        .find(|v| {
            aster::coordination::ids::CoordinationNodeId::parse_hex(v.node().id().as_str())
                .expect("a voter id")
                != leader_id
        })
        .expect("two of three are followers");

    let client = Client::connect([0xD9 ^ 0x42; 32], &fixture, follower).await;
    let (code, _, details) = call_refused(
        &client,
        "group_status",
        &GroupStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: fixture.group_id().as_bytes().to_vec(),
            // Any request shape works: stage 1 answers before authorization.
        },
    )
    .await;
    assert_eq!(code, UNAVAILABLE, "a follower does not serve Store");
    let hint = details
        .iter()
        .find(|(k, _)| k == DETAIL_LEADER_NODE_ID)
        .map(|(_, v)| v.clone())
        .expect("the refusal carries the leader hint");
    assert_eq!(
        hint,
        leader_id.to_hex(),
        "the hint names the leader every voter agrees on"
    );

    // And the leader answers the identical request the follower refused —
    // as a permission denial, because the client is a stranger, which is
    // still not an oracle about anything replicated.
    let leader = voters
        .iter()
        .find(|v| {
            aster::coordination::ids::CoordinationNodeId::parse_hex(v.node().id().as_str())
                .expect("a voter id")
                == leader_id
        })
        .expect("the leader is among the voters");
    let client_at_leader = Client::connect([0xD9 ^ 0x42; 32], &fixture, leader).await;
    let (code, _, _) = call_refused(
        &client_at_leader,
        "group_status",
        &GroupStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: fixture.group_id().as_bytes().to_vec(),
        },
    )
    .await;
    assert_eq!(code, PERMISSION_DENIED);

    for voter in voters {
        voter.shutdown().await;
    }
}

/// **read_linearizable of a never-written register** is its value — epoch 0,
/// unheld, an empty present value — not an error and not an oracle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_never_written_register_reads_as_its_default_value() {
    let h = harness(0xDA).await;
    let group = group_of(&h.fixture);
    grant(&h, 1, b"empty", h.worker.id, RIGHT_READ, 0).await;

    let read: ReadReplyWireV1 = call(
        &h.worker,
        "read_linearizable",
        &ReadRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: group,
            resource: resource(b"empty"),
        },
    )
    .await;
    // The §9.1 detail: `set_grant` lazily created the row to carry its ACL
    // revision, so the register exists with revision 1 and no value bytes.
    assert_eq!(read.snapshot.epoch, 0);
    assert!(!read.snapshot.held);
    assert_eq!(read.snapshot.holder, None);
    assert_eq!(read.snapshot.value, Some(Vec::new()));
    assert!(!read.snapshot.value_redacted);
}
