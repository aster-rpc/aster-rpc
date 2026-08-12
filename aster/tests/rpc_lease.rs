#![cfg(feature = "rpc")]
//! `aster.lease.Grantor`: the designated-grantor serialization point over
//! RPC. Covers holder binding to the authenticated peer (the wire cannot
//! impersonate a holder), grant/deny/release over the wire, Gate-3 gating of
//! `revoke`, co-located enforcement at the grantor's store, and the
//! `LeaseHandle` renew loop running end-to-end against a remote grantor.

use std::sync::Arc;
use std::time::Duration;

use aster::lease::{
    AcquireOutcome, DenyReason, FenceRejection, LeaseHandle, LeaseOp, LeaseOptions,
    LeaseSerializer, LeaseStatus, MemorySerializer, TransitionOutcome,
};
use aster::rpc::lease::{GrantorSerializer, LeaseGrantor};
use aster::rpc::{AsterServer, AttributeStore};
use aster::{AsterConfig, Node, RelayMode};
use tokio::time::timeout;

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

async fn connect(client: &Node, srv: &AsterServer) -> aster::rpc::RpcConnection {
    wait_for_addr(srv.node()).await;
    wait_for_addr(client).await;
    client.add_peer(srv.node()).unwrap();
    srv.node().add_peer(client).unwrap();
    within(client.rpc_connect(&srv.id())).await.unwrap()
}

const TTL: Duration = Duration::from_secs(30);

async fn acquire(s: &GrantorSerializer, id: &str, resource: &str) -> TransitionOutcome {
    s.transition(
        resource,
        LeaseOp::Acquire {
            candidate: id.into(),
            ttl: TTL,
        },
    )
    .await
    .unwrap()
}

/// Grant, deny-for-the-loser, holder-bound renew/release, co-located
/// enforcement — the wire cannot impersonate a holder.
#[tokio::test]
async fn grantor_binds_ops_to_authenticated_caller() {
    let store = MemorySerializer::new();
    let server = AsterServer::builder()
        .config(cfg())
        .service(LeaseGrantor::new(Arc::new(store.clone())))
        .start()
        .await
        .unwrap();
    let node_a = Node::start(cfg()).await.unwrap();
    let node_b = Node::start(cfg()).await.unwrap();
    let (a_id, b_id) = (node_a.id().to_string(), node_b.id().to_string());

    let grantor_a = GrantorSerializer::new(connect(&node_a, &server).await, a_id.clone());
    let grantor_b = GrantorSerializer::new(connect(&node_b, &server).await, b_id.clone());

    // A wins epoch 1; B is denied with the row visible for backoff.
    let fence = match acquire(&grantor_a, &a_id, "tree/1").await {
        TransitionOutcome::Granted(f) => f,
        other => panic!("expected grant, got {other:?}"),
    };
    assert_eq!(fence.epoch, 1);
    assert_eq!(fence.holder, a_id);
    match acquire(&grantor_b, &b_id, "tree/1").await {
        TransitionOutcome::Denied(DenyReason::HeldByOther, snap) => {
            assert_eq!(snap.holder.as_deref(), Some(a_id.as_str()));
            assert!(snap.remaining.unwrap() > Duration::ZERO);
        }
        other => panic!("expected HeldByOther, got {other:?}"),
    }

    // Holder binding: B sends a renew for A's epoch. The client-side check
    // is bypassed by building B's serializer with A's id — the SERVER must
    // still bind the op to B's authenticated identity and refuse.
    let impersonator = GrantorSerializer::new(connect(&node_b, &server).await, a_id.clone());
    let renew = impersonator
        .transition(
            "tree/1",
            LeaseOp::Renew {
                holder: a_id.clone(),
                epoch: fence.epoch,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(renew, TransitionOutcome::Denied(DenyReason::NotHolder, _)),
        "server accepted an impersonated renew: {renew:?}"
    );

    // Co-located enforcement at the grantor's own store: the authenticated
    // writer identity decides, not the fence bytes.
    assert!(store.mutate_fenced(&fence, &a_id, || ()).is_ok());
    assert_eq!(
        store.mutate_fenced(&fence, &b_id, || ()).unwrap_err(),
        FenceRejection::NotHolder
    );

    // A's real renew and release work; release frees the row for B at a
    // higher epoch (the fence advanced twice: release + re-grant).
    let renew = grantor_a
        .transition(
            "tree/1",
            LeaseOp::Renew {
                holder: a_id.clone(),
                epoch: fence.epoch,
            },
        )
        .await
        .unwrap();
    assert!(matches!(renew, TransitionOutcome::Renewed));
    let release = grantor_a
        .transition(
            "tree/1",
            LeaseOp::Release {
                holder: a_id.clone(),
                epoch: fence.epoch,
            },
        )
        .await
        .unwrap();
    assert!(matches!(release, TransitionOutcome::Released));
    match acquire(&grantor_b, &b_id, "tree/1").await {
        TransitionOutcome::Granted(f) => assert_eq!(f.epoch, 3),
        other => panic!("expected grant after release, got {other:?}"),
    }

    // The released holder's stale fence is dead at the enforcement point.
    assert!(matches!(
        store.mutate_fenced(&fence, &a_id, || ()).unwrap_err(),
        FenceRejection::StaleEpoch { .. }
    ));
}

/// `revoke` is Gate-3 gated (operator by default): denied without the role,
/// and once granted it advances the fence at the store.
#[tokio::test]
async fn revoke_requires_operator_role() {
    let store = MemorySerializer::new();
    let attrs = AttributeStore::new();
    let server = AsterServer::builder()
        .config(cfg())
        .service(LeaseGrantor::new(Arc::new(store.clone())))
        .attributes(attrs.clone())
        .start()
        .await
        .unwrap();
    let holder = Node::start(cfg()).await.unwrap();
    let authority = Node::start(cfg()).await.unwrap();
    let (h_id, auth_id) = (holder.id().to_string(), authority.id().to_string());

    let grantor_h = GrantorSerializer::new(connect(&holder, &server).await, h_id.clone());
    let grantor_auth = GrantorSerializer::new(connect(&authority, &server).await, auth_id.clone());

    let fence = match acquire(&grantor_h, &h_id, "db/primary").await {
        TransitionOutcome::Granted(f) => f,
        other => panic!("expected grant, got {other:?}"),
    };

    // Without the operator role the revoke is rejected at Gate 3 (an RPC
    // error, not a lease deny — the state machine is never reached).
    let err = grantor_auth
        .transition(
            "db/primary",
            LeaseOp::Revoke {
                authority: auth_id.clone(),
            },
        )
        .await;
    assert!(err.is_err(), "ungated revoke must fail, got {err:?}");
    assert!(store.mutate_fenced(&fence, &h_id, || ()).is_ok());

    // Grant the role; revoke passes and the fence advances immediately.
    attrs.set_role(&auth_id, "operator");
    let revoked = grantor_auth
        .transition(
            "db/primary",
            LeaseOp::Revoke {
                authority: auth_id.clone(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(revoked, TransitionOutcome::Revoked));
    assert!(matches!(
        store.mutate_fenced(&fence, &h_id, || ()).unwrap_err(),
        FenceRejection::StaleEpoch { .. }
    ));
}

/// The full holder engine against a remote grantor: background renewals keep
/// the lease alive past several TTLs; a snapshot round-trips; release frees.
#[tokio::test]
async fn lease_handle_renews_over_rpc() {
    let store = MemorySerializer::new();
    let server = AsterServer::builder()
        .config(cfg())
        .service(LeaseGrantor::new(Arc::new(store)))
        .start()
        .await
        .unwrap();
    let node = Node::start(cfg()).await.unwrap();
    let id = node.id().to_string();
    let serializer = Arc::new(GrantorSerializer::new(
        connect(&node, &server).await,
        id.clone(),
    ));

    let opts = LeaseOptions {
        ttl: Duration::from_secs(2),
        renew_interval: Duration::from_millis(500),
        holder_margin: Duration::from_millis(250),
    };
    let lease = match within(LeaseHandle::acquire(
        serializer.clone(),
        "session/9",
        &id,
        opts,
    ))
    .await
    .unwrap()
    {
        AcquireOutcome::Granted(l) => l,
        AcquireOutcome::Denied(r, _) => panic!("denied: {r:?}"),
    };

    // Live past 2.5 × ttl — only possible if renewals flow over the wire.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(lease.status(), LeaseStatus::Held);
    assert!(lease.fence().is_some());

    let snap = serializer.snapshot("session/9").await.unwrap();
    assert_eq!(snap.holder.as_deref(), Some(id.as_str()));
    assert!(snap.held);

    within(lease.release()).await.unwrap();
    let snap = serializer.snapshot("session/9").await.unwrap();
    assert!(!snap.held);
    assert_eq!(snap.epoch, 2);
}
