#![cfg(all(feature = "binding-support", feature = "rpc"))]
//! Authentication upcalls remain bounded after polling and never turn missing
//! or malformed answers into authenticated calls, regardless of failure policy.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aster::binding_support::upcall::auth::{ForeignAuthenticator, ForeignAuthenticatorOptions};
use aster::binding_support::upcall::codec::{self, RpcAuthResponse};
use aster::binding_support::upcall::{
    CancelReason, HookFailureMode, UpcallEvent, UpcallKind, UpcallPolicy, UpcallQueue,
    UpcallStatus, Upcalls,
};
use aster::rpc::{AuthContext, AuthOutcome, Authenticator, RpcStatus, StatusCode};
use aster_transport_core::canonical as c;

fn context() -> AuthContext {
    AuthContext {
        peer_id: "127.0.0.1:1234".into(),
        service: "Counter".into(),
        method: "read".into(),
        session_id: 7,
        metadata: HashMap::from([("authorization".into(), "Bearer secret".into())]),
    }
}

fn setup(capacity: usize) -> (Arc<UpcallQueue>, Upcalls, ForeignAuthenticator) {
    let queue = UpcallQueue::new(capacity);
    let upcalls = Upcalls::new(queue.clone());
    let auth = ForeignAuthenticator::with_options(
        upcalls.clone(),
        ForeignAuthenticatorOptions {
            max_pending: 1,
            max_payload_bytes: 1024,
        },
    )
    .unwrap();
    (queue, upcalls, auth)
}

fn start(auth: ForeignAuthenticator) -> tokio::task::JoinHandle<Result<AuthOutcome, RpcStatus>> {
    tokio::spawn(async move { auth.authenticate(&context()).await })
}

async fn next(queue: &UpcallQueue) -> UpcallEvent {
    tokio::time::timeout(Duration::from_secs(1), queue.ready())
        .await
        .unwrap();
    let mut event = None;
    queue.drain(1, |value| event = Some(value));
    event.unwrap()
}

fn accepted() -> Vec<u8> {
    codec::encode_rpc_auth_response(&RpcAuthResponse::Authenticated {
        principal: "user/alice".into(),
        attributes: HashMap::from([("aster.role".into(), "operator".into())]),
    })
    .unwrap()
}

#[tokio::test]
async fn explicit_principal_and_attributes_roundtrip_without_changing_transport_hint() {
    let (queue, upcalls, auth) = setup(4);
    let task = start(auth);
    let event = next(&queue).await;
    assert_eq!(event.kind, UpcallKind::RpcAuthenticate);
    assert_eq!(event.kind.code(), 80);
    assert!(!event.kind.is_decision());
    let request = codec::decode_rpc_auth_request(&event.payload).unwrap();
    assert_eq!(request.peer_id, context().peer_id);
    assert_eq!(request.service, "Counter");
    assert_eq!(request.method, "read");
    assert_eq!(request.session_id, 7);
    assert_eq!(request.metadata, context().metadata);
    upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, accepted())
        .unwrap();
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.principal.as_deref(), Some("user/alice"));
    assert_eq!(result.attributes["aster.role"], "operator");
    assert_eq!(upcalls.pending_count(), 0);
}

#[tokio::test]
async fn draining_queue_does_not_release_pending_capacity_and_clones_share_limit() {
    let (queue, upcalls, auth) = setup(4);
    let waiting = start(auth.clone());
    let event = next(&queue).await;
    assert_eq!(queue.len(), 0);
    assert_eq!(upcalls.pending_count(), 1);
    let refusal = tokio::time::timeout(Duration::from_millis(100), auth.authenticate(&context()))
        .await
        .unwrap()
        .err()
        .unwrap();
    assert_eq!(refusal.code, StatusCode::ResourceExhausted.as_i32());
    assert_eq!(upcalls.pending_count(), 1);
    upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, accepted())
        .unwrap();
    waiting.await.unwrap().unwrap();
    let reused = start(auth);
    let event = next(&queue).await;
    upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, accepted())
        .unwrap();
    reused.await.unwrap().unwrap();
}

#[tokio::test]
async fn future_drop_cancels_foreign_work_and_returns_capacity() {
    let (queue, upcalls, auth) = setup(4);
    let abandoned = start(auth.clone());
    let event = next(&queue).await;
    abandoned.abort();
    assert!(matches!(abandoned.await, Err(error) if error.is_cancelled()));
    let cancel = next(&queue).await;
    assert_eq!(cancel.kind, UpcallKind::Cancel);
    assert_eq!(cancel.invocation_id, event.invocation_id);
    assert_eq!(cancel.cancel_reason(), Some(CancelReason::Abandoned));
    assert!(upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, accepted())
        .is_err());
    assert_eq!(upcalls.pending_count(), 0);
    let reused = start(auth);
    let event = next(&queue).await;
    upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, accepted())
        .unwrap();
    reused.await.unwrap().unwrap();
}

#[tokio::test]
async fn refusal_invalid_answers_and_absent_handlers_never_fail_open() {
    let (queue, upcalls, auth) = setup(4);
    upcalls.set_policy(
        UpcallKind::RpcAuthenticate,
        UpcallPolicy::new(Duration::from_secs(1), HookFailureMode::FailOpen),
    );
    for (status, bytes, expected) in [
        (
            UpcallStatus::Ok,
            codec::encode_rpc_auth_response(&RpcAuthResponse::Rejected).unwrap(),
            StatusCode::Unauthenticated,
        ),
        (UpcallStatus::Ok, Vec::new(), StatusCode::Unauthenticated),
        // accepted=true followed by empty principal and empty attributes
        (
            UpcallStatus::Ok,
            vec![0xCA, 1, 1, 2, 0, 0x0C],
            StatusCode::Unauthenticated,
        ),
        (
            UpcallStatus::Unimplemented,
            Vec::new(),
            StatusCode::Unauthenticated,
        ),
        (
            UpcallStatus::Error,
            b"Bearer secret".to_vec(),
            StatusCode::Unavailable,
        ),
    ] {
        let task = start(auth.clone());
        let event = next(&queue).await;
        upcalls.respond(event.invocation_id, status, bytes).unwrap();
        let error = task.await.unwrap().err().unwrap();
        assert_eq!(error.code, expected.as_i32());
        assert!(!error.message.contains("secret"));
        assert_eq!(upcalls.pending_count(), 0);
    }
}

#[tokio::test]
async fn timeout_and_closed_upcalls_fail_closed_and_settle_pending_work() {
    let (queue, upcalls, auth) = setup(4);
    upcalls.set_policy(
        UpcallKind::RpcAuthenticate,
        UpcallPolicy::new(Duration::from_millis(10), HookFailureMode::FailOpen),
    );
    let task = start(auth.clone());
    let event = next(&queue).await;
    assert_eq!(
        task.await.unwrap().err().unwrap().code,
        StatusCode::Unavailable.as_i32()
    );
    let cancel = next(&queue).await;
    assert_eq!(cancel.invocation_id, event.invocation_id);
    assert_eq!(cancel.cancel_reason(), Some(CancelReason::Timeout));
    assert_eq!(upcalls.pending_count(), 0);
    // A timed-out callback released the adapter's sole permit.
    let task = start(auth.clone());
    let event = next(&queue).await;
    upcalls.close();
    assert_eq!(
        task.await.unwrap().err().unwrap().code,
        StatusCode::Unavailable.as_i32()
    );
    assert!(upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, accepted())
        .is_err());
    assert_eq!(
        auth.authenticate(&context()).await.err().unwrap().code,
        StatusCode::Unavailable.as_i32()
    );
    assert_eq!(upcalls.pending_count(), 0);
}

#[tokio::test]
async fn queue_overload_is_refused_and_does_not_keep_capacity() {
    let (queue, upcalls, auth) = setup(1);
    let filling = tokio::spawn({
        let upcalls = upcalls.clone();
        async move { upcalls.call(UpcallKind::LeaseSnapshot, vec![]).await }
    });
    queue.ready().await;
    assert_eq!(
        auth.authenticate(&context()).await.err().unwrap().code,
        StatusCode::ResourceExhausted.as_i32()
    );
    let event = next(&queue).await;
    upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, vec![])
        .unwrap();
    filling.await.unwrap().unwrap();
    let task = start(auth);
    let event = next(&queue).await;
    upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, accepted())
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn encoded_payload_limits_are_checked_before_request_publication_and_response_decode() {
    let (queue, upcalls, _) = setup(4);
    let ctx = context();
    let exact = codec::encode_rpc_auth_request(
        &ctx.peer_id,
        &ctx.service,
        &ctx.method,
        ctx.session_id,
        &ctx.metadata,
    )
    .len();
    let options = |limit| ForeignAuthenticatorOptions {
        max_pending: 1,
        max_payload_bytes: limit,
    };
    let too_small =
        ForeignAuthenticator::with_options(upcalls.clone(), options(exact - 1)).unwrap();
    assert_eq!(
        too_small.authenticate(&ctx).await.err().unwrap().code,
        StatusCode::ResourceExhausted.as_i32()
    );
    assert_eq!(queue.len(), 0);
    assert_eq!(upcalls.pending_count(), 0);
    let exact_fit = ForeignAuthenticator::with_options(upcalls.clone(), options(exact)).unwrap();
    let task = start(exact_fit.clone());
    let event = next(&queue).await;
    assert_eq!(event.payload.len(), exact);
    upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, vec![0xCA; exact + 1])
        .unwrap();
    assert_eq!(
        task.await.unwrap().err().unwrap().code,
        StatusCode::Unauthenticated.as_i32()
    );
    let task = start(exact_fit);
    let event = next(&queue).await;
    upcalls
        .respond(event.invocation_id, UpcallStatus::Ok, accepted())
        .unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn malformed_authentication_payloads_and_noncanonical_maps_are_rejected() {
    let request =
        codec::encode_rpc_auth_request("peer", "svc", "method", i32::MIN, &HashMap::new());
    assert_eq!(
        codec::decode_rpc_auth_request(&request).unwrap().session_id,
        i32::MIN
    );
    let response = accepted();
    for end in 0..request.len() {
        assert!(codec::decode_rpc_auth_request(&request[..end]).is_err());
    }
    for end in 0..response.len() {
        assert!(codec::decode_rpc_auth_response(&response[..end]).is_err());
    }
    for names in [["b", "a"], ["a", "a"]] {
        let mut bytes = vec![0xCA, 1, 1];
        c::write_string(&mut bytes, "principal");
        c::write_list_header(&mut bytes, 2);
        for name in names {
            c::write_string(&mut bytes, name);
            c::write_string(&mut bytes, "value");
        }
        assert!(codec::decode_rpc_auth_response(&bytes).is_err());
    }
    assert!(codec::decode_rpc_auth_response(&[0xCA, 1, 0, 0]).is_err());
    assert!(codec::decode_rpc_auth_response(&[0xCA, 1, 2]).is_err());
    assert!(codec::decode_rpc_auth_response(&[0xCA, 1, 1, 0x82, 0, 0, 0x0C]).is_err());
    assert!(
        codec::encode_rpc_auth_response(&RpcAuthResponse::Authenticated {
            principal: String::new(),
            attributes: HashMap::new(),
        })
        .is_err()
    );
}

#[test]
fn invalid_resource_limits_are_errors_instead_of_panics() {
    let (_, upcalls, _) = setup(1);
    for options in [
        ForeignAuthenticatorOptions {
            max_pending: 0,
            max_payload_bytes: 1,
        },
        ForeignAuthenticatorOptions {
            max_pending: usize::MAX,
            max_payload_bytes: 1,
        },
        ForeignAuthenticatorOptions {
            max_pending: 1,
            max_payload_bytes: 0,
        },
    ] {
        assert!(ForeignAuthenticator::with_options(upcalls.clone(), options).is_err());
    }
}
