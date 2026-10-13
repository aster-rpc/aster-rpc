#![cfg(all(feature = "enrollment", feature = "docs"))]
//! Root pre-rotation over the network (enrollment B12): INV-A16, INV-A17 and
//! INV-E20. The root R0 is assumed compromised; the pre-committed R1 rotates,
//! republishes a node's registration into a fresh fleet namespace, and the
//! fleet follows: a replica that applies the statement, a node that was away
//! during the rotation and learns it in the handshake, and a restart that
//! keeps the new root.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::delegation::{issue_root_rotation, next_root_commitment, RotationReason};
use aster::enrollment::*;
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey};

const FLEET: &str = "root-rotation-test";
const R0: u8 = 1;
const R1: u8 = 11;
const R2: u8 = 12;
const REPLICA: u8 = 2;
const MEMBER: u8 = 3;
const STRANGER: u8 = 9;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn key(seed: u8) -> SecretKey {
    SecretKey::from_bytes([seed; 32])
}
fn public(seed: u8) -> [u8; 32] {
    JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
}
fn token(root: u8, seed: u8) -> String {
    issue_credential(
        &key(root),
        &EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600),
        now(),
    )
    .unwrap()
    .into_chain()
}
fn namespace(doc: &Doc) -> [u8; 32] {
    hex::decode(doc.id().unwrap().to_string())
        .unwrap()
        .try_into()
        .unwrap()
}
async fn node(seed: u8) -> Node {
    Node::start_with_alpns(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(key(seed))
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed)
            .hook_timeout_ms(1000)
            .build(),
        vec![ENROLLMENT_ALPN.to_vec()],
    )
    .await
    .unwrap()
}
/// Pins `root` with the commitment to the next root and its rotation count.
fn enrollment(
    node: &Node,
    root: u8,
    next: u8,
    seq: u64,
    namespace: [u8; 32],
    path: &std::path::Path,
    credential: Option<String>,
) -> anyhow::Result<Enrollment> {
    Enrollment::install(
        node,
        Arc::new(EnrollmentPolicyStore::open(
            path,
            public(root),
            namespace,
            FLEET,
        )?),
        JwtEnrollmentVerifier::new(public(root), FLEET)?,
        credential,
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
        EnrollmentOptions {
            reconciliation_interval: Duration::from_millis(50),
            rotation: Some(RotationPin {
                next_commitment: next_root_commitment(&public(next)),
                seq,
            }),
            ..Default::default()
        },
    )
}
fn serve(node: Node, enrollment: Enrollment) {
    tokio::spawn(async move {
        while let Ok((alpn, connection)) = node.accept().await {
            if alpn == ENROLLMENT_ALPN {
                let enrollment = enrollment.clone();
                tokio::spawn(async move {
                    let _ = enrollment.accept(&connection).await;
                    connection.closed().await;
                });
            }
        }
    });
}
fn registration(root: u8, subject: u8, revision: u64) -> Vec<u8> {
    serde_json::to_vec(&NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(public(root)),
        subject: hex::encode(public(subject)),
        audience: Audience::One(FLEET.into()),
        revision,
        issued_at: now(),
        not_before: None,
        expires_at: None,
        status: NodeRegistrationStatus::Active,
        claims: AdmissionClaims::default(),
        mandatory: vec![],
        reauthorizes_revision: None,
        reason: None,
    })
    .unwrap()
}
async fn put(root: &Node, doc: &Doc, key: Vec<u8>, value: Vec<u8>) {
    doc.set_bytes(&root.docs().default_author().await.unwrap(), key, value)
        .await
        .unwrap();
}
async fn sync(enrollment: &Enrollment, addr: aster::NodeAddr) -> EnrollmentReadiness {
    let (ready, ticket) = enrollment.connect(addr).await.unwrap();
    enrollment
        .synchronize(ready.peer, ticket.unwrap(), Duration::from_secs(15))
        .await
        .unwrap();
    ready
}

#[tokio::test]
async fn fleet_survives_a_compromise_rotation_and_restart() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let files = tempfile::tempdir().unwrap();
        // The original root R0 and its fleet namespace.
        let old_root = node(R0).await;
        let old_fleet = old_root.docs().create().await.unwrap();
        let old_namespace = namespace(&old_fleet);
        // The compromised root also registered a stranger.
        put(
            &old_root,
            &old_fleet,
            node_registration_key(&public(STRANGER)),
            registration(R0, STRANGER, 1),
        )
        .await;
        let old_enroll = enrollment(
            &old_root,
            R0,
            R1,
            0,
            old_namespace,
            &files.path().join("old-root.json"),
            Some(token(R0, R0)),
        )
        .unwrap();
        old_enroll.set_policy_document(&old_fleet).unwrap();
        serve(old_root.clone(), old_enroll);

        let replica = node(REPLICA).await;
        let replica_path = files.path().join("replica.json");
        let replica_enroll = enrollment(
            &replica,
            R0,
            R1,
            0,
            old_namespace,
            &replica_path,
            Some(token(R0, REPLICA)),
        )
        .unwrap();
        serve(replica.clone(), replica_enroll.clone());
        sync(&replica_enroll, old_root.addr()).await;

        let member = node(MEMBER).await;
        let member_path = files.path().join("member.json");
        let member_enroll = enrollment(
            &member,
            R0,
            R1,
            0,
            old_namespace,
            &member_path,
            Some(token(R0, MEMBER)),
        )
        .unwrap();
        sync(&member_enroll, replica.addr()).await;
        assert!(replica_enroll.check_peer(&public(MEMBER)).await.is_ok());
        let mut revocations = replica_enroll.revocations();

        // R1 takes over: a fresh fleet namespace with the member's
        // republished registration, a rotation statement naming it, and a
        // new credential for the replica.
        let new_root = node(R1).await;
        let new_fleet = new_root.docs().create().await.unwrap();
        let new_namespace = namespace(&new_fleet);
        put(
            &new_root,
            &new_fleet,
            node_registration_key(&public(MEMBER)),
            registration(R1, MEMBER, 1),
        )
        .await;
        let statement = issue_root_rotation(
            &key(R1),
            &aster::PublicKey::from_bytes(public(R0)),
            1,
            &next_root_commitment(&public(R2)),
            &[new_namespace],
            now(),
            RotationReason::Compromise,
        )
        .unwrap();
        let new_enroll = enrollment(
            &new_root,
            R1,
            R2,
            1,
            new_namespace,
            &files.path().join("new-root.json"),
            Some(token(R1, R1)),
        )
        .unwrap();
        new_enroll.set_policy_document(&new_fleet).unwrap();
        serve(new_root.clone(), new_enroll);

        // INV-A16: the compromised R0 cannot rotate, and neither can a
        // statement with the wrong previous root or sequence.
        let forged = issue_root_rotation(
            &key(R0),
            &aster::PublicKey::from_bytes(public(R0)),
            1,
            &next_root_commitment(&public(STRANGER)),
            &[old_namespace],
            now(),
            RotationReason::Routine,
        )
        .unwrap();
        assert!(replica_enroll.apply_root_rotations(&[forged]).is_err());
        let wrong_prev = issue_root_rotation(
            &key(R1),
            &aster::PublicKey::from_bytes(public(STRANGER)),
            1,
            &next_root_commitment(&public(R2)),
            &[new_namespace],
            now(),
            RotationReason::Compromise,
        )
        .unwrap();
        assert!(replica_enroll.apply_root_rotations(&[wrong_prev]).is_err());
        let wrong_seq = issue_root_rotation(
            &key(R1),
            &aster::PublicKey::from_bytes(public(R0)),
            2,
            &next_root_commitment(&public(R2)),
            &[new_namespace],
            now(),
            RotationReason::Compromise,
        )
        .unwrap();
        assert!(replica_enroll.apply_root_rotations(&[wrong_seq]).is_err());
        assert_eq!(replica_enroll.root_state().unwrap().root, public(R0));

        // The replica applies the real statement out of band.
        assert_eq!(
            replica_enroll
                .apply_root_rotations(std::slice::from_ref(&statement))
                .unwrap(),
            1
        );
        let state = replica_enroll.root_state().unwrap();
        assert_eq!(state.root, public(R1));
        assert_eq!(state.next_commitment, next_root_commitment(&public(R2)));
        assert_eq!(state.seq, 1);
        // A replay is not applied twice.
        assert_eq!(
            replica_enroll
                .apply_root_rotations(std::slice::from_ref(&statement))
                .unwrap(),
            0
        );
        // INV-A17: every session rested on R0 and is closed.
        let revoked = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = revocations.recv().await.unwrap();
                if event.peer == public(MEMBER) {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(revoked.reason, DenialReason::RootRotated);
        assert!(replica_enroll.check_peer(&public(MEMBER)).await.is_err());

        // The replica takes a credential from R1 and syncs the new fleet.
        replica_enroll
            .replace_credential(token(R1, REPLICA))
            .await
            .unwrap();
        sync(&replica_enroll, new_root.addr()).await;

        // INV-E20: neither an R0 chain nor an R0-era registration admits
        // anyone now.
        let stranger = node(STRANGER).await;
        let stranger_enroll = enrollment(
            &stranger,
            R0,
            R1,
            0,
            old_namespace,
            &files.path().join("stranger.json"),
            Some(token(R0, STRANGER)),
        )
        .unwrap();
        assert!(stranger_enroll.connect(replica.addr()).await.is_err());
        assert!(replica_enroll.check_peer(&public(STRANGER)).await.is_err());

        // The member was away during the rotation. Its next handshake
        // delivers the statement, and its republished registration admits
        // it under R1 even though its own R0 credential no longer counts.
        let ready = sync(&member_enroll, replica.addr()).await;
        assert_eq!(ready.peer, public(REPLICA));
        assert_eq!(member_enroll.root_state().unwrap().root, public(R1));
        assert_eq!(member_enroll.root_rotations(), vec![statement.clone()]);
        assert!(matches!(
            replica_enroll.check_peer(&public(MEMBER)).await.unwrap().source,
            AdmissionEvidenceSource::RootAuthoredRecord { document_id, .. }
                if document_id == new_namespace
        ));

        // A restart with the original R0 pin resumes the journaled
        // rotation: the node trusts R1 before talking to anyone, keeps its
        // key, and is admitted again on its record.
        drop(member_enroll);
        member.shutdown().await;
        let member = node(MEMBER).await;
        let member_enroll = enrollment(
            &member,
            R0,
            R1,
            0,
            old_namespace,
            &member_path,
            Some(token(R0, MEMBER)),
        )
        .unwrap();
        let state = member_enroll.root_state().unwrap();
        assert_eq!((state.root, state.seq), (public(R1), 1));
        sync(&member_enroll, replica.addr()).await;
        assert!(matches!(
            replica_enroll
                .check_peer(&public(MEMBER))
                .await
                .unwrap()
                .source,
            AdmissionEvidenceSource::RootAuthoredRecord { .. }
        ));

        member.shutdown().await;
        stranger.shutdown().await;
        replica.shutdown().await;
        new_root.shutdown().await;
        old_root.shutdown().await;
    })
    .await
    .unwrap();
}

/// A consumer that later pins the rotated root directly (for example after
/// `aster trust rotate --apply` updated its profile) reopens the same store
/// path: the previous root's state is never read for the new root.
#[test]
fn stores_are_kept_per_root_at_one_path() {
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("fleet.json");
    let old = EnrollmentPolicyStore::open(&path, public(R0), [1; 32], FLEET).unwrap();
    let rotated = old.open_rotated(public(R1), [2; 32]).unwrap();
    drop((old, rotated));
    let reopened = EnrollmentPolicyStore::open(&path, public(R1), [2; 32], FLEET).unwrap();
    assert_eq!(reopened.root(), &public(R1));
    assert_eq!(reopened.namespace(), &[2; 32]);
    drop(reopened);
    // A root whose namespace differs from what was kept for it is refused.
    assert!(EnrollmentPolicyStore::open(&path, public(R1), [3; 32], FLEET).is_err());
}
