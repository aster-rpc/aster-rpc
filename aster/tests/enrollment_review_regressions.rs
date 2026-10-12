#![cfg(all(feature = "enrollment", feature = "docs"))]
//! Regressions from the 2026-10-12 enrollment review.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::{AsterConfig, Node, RelayMode, SecretKey};
use tokio::sync::mpsc;

const NORMAL: &[u8] = b"enrollment.application.test/1";
const FLEET: &str = "offline-root-test";

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
fn token(seed: u8) -> String {
    issue_jwt(
        &key(1),
        &EnrollmentIssueRequest::new(public(seed), FLEET),
        now(),
    )
    .unwrap()
    .into_jwt()
}
async fn node(seed: u8, hooks: bool) -> Node {
    let builder = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(key(seed));
    let builder = if hooks {
        builder
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed)
            .hook_timeout_ms(1000)
    } else {
        builder
    };
    Node::start_with_alpns(
        builder.build(),
        vec![ENROLLMENT_ALPN.to_vec(), NORMAL.to_vec()],
    )
    .await
    .unwrap()
}
fn enrollment(node: &Node, path: &std::path::Path, seed: u8, permits: usize) -> Enrollment {
    enrollment_with_options(
        node,
        path,
        seed,
        EnrollmentOptions {
            max_concurrent_handshakes: permits,
            ..Default::default()
        },
    )
}
fn enrollment_with_options(
    node: &Node,
    path: &std::path::Path,
    seed: u8,
    options: EnrollmentOptions,
) -> Enrollment {
    Enrollment::install(
        node,
        Arc::new(EnrollmentPolicyStore::open(path, public(1), [42; 32], FLEET).unwrap()),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        Some(token(seed)),
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
        options,
    )
    .unwrap()
}

#[tokio::test]
async fn admitted_peer_capacity_preserves_existing_member() {
    let files = tempfile::tempdir().unwrap();
    let replica = node(2, true).await;
    let replica_enroll = enrollment_with_options(
        &replica,
        &files.path().join("replica.json"),
        2,
        EnrollmentOptions {
            max_admitted_peers: 1,
            ..Default::default()
        },
    );
    let mut received = serve(replica.clone(), replica_enroll.clone());
    let member = node(3, true).await;
    let member_enroll = enrollment(&member, &files.path().join("member.json"), 3, 16);
    member_enroll.connect(replica.addr()).await.unwrap();
    let extra = node(4, true).await;
    let extra_enroll = enrollment(&extra, &files.path().join("extra.json"), 4, 16);
    assert!(extra_enroll.connect(replica.addr()).await.is_err());
    assert!(replica_enroll.check_peer(&public(4)).await.is_err());
    replica_enroll.check_peer(&public(3)).await.unwrap();
    member.add_peer(&replica).unwrap();
    let _connection = member.connect(&replica.id(), NORMAL).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), received.recv())
            .await
            .unwrap()
            .is_some()
    );
    extra.shutdown().await;
    member.shutdown().await;
    replica.shutdown().await;
}
fn serve(node: Node, enrollment: Enrollment) -> mpsc::Receiver<aster::Connection> {
    let (send, recv) = mpsc::channel(16);
    tokio::spawn(async move {
        while let Ok((alpn, connection)) = node.accept().await {
            if alpn == ENROLLMENT_ALPN {
                let enrollment = enrollment.clone();
                tokio::spawn(async move {
                    let _ = enrollment.accept(&connection).await;
                });
            } else {
                let _ = send.send(connection).await;
            }
        }
    });
    recv
}

/// Unauthenticated peers may only reach the enrollment ALPN, but stalled
/// enrollment handshakes share the permit pool that Gate-0 uses for admitted
/// peers' ordinary connections. Filling it must not lock members out.
#[tokio::test]
async fn stalled_unauthenticated_handshakes_cannot_starve_admitted_peers() {
    let files = tempfile::tempdir().unwrap();
    let replica = node(2, true).await;
    let replica_enroll = enrollment(&replica, &files.path().join("replica.json"), 2, 1);
    let mut received = serve(replica.clone(), replica_enroll.clone());

    let client = node(3, true).await;
    let client_enroll = enrollment(&client, &files.path().join("client.json"), 3, 16);
    client_enroll.connect(replica.addr()).await.unwrap();

    // An unknown peer opens the restricted ALPN and never speaks.
    let attacker = node(8, false).await;
    attacker.add_peer(&replica).unwrap();
    let _stalled = attacker
        .connect(&replica.id(), ENROLLMENT_ALPN)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    client.add_peer(&replica).unwrap();
    let _connection = client.connect(&replica.id(), NORMAL).await;
    let accepted = tokio::time::timeout(Duration::from_secs(3), received.recv()).await;
    assert!(
        matches!(accepted, Ok(Some(_))),
        "admitted peer's ordinary connection was rejected while an unauthenticated handshake held the permit"
    );
    attacker.shutdown().await;
    client.shutdown().await;
    replica.shutdown().await;
}
