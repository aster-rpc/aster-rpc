#![cfg(feature = "rpc")]
//! Gate-3 (per-call capability) auth: a method that `requires` a role is
//! rejected with PERMISSION_DENIED until the application injects that role for
//! the caller into the server's AttributeStore; an ungated method always works.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aster::rpc::{
    require_role, AttributeStore, AuthContext, AuthOutcome, Authenticator, Call, CallParts,
    CapabilityRequirement, Dispatcher, OutgoingFrame, PeerProvenance, RequestFrame, RpcConnection,
    RpcStatus, SerializationMode, Server, ServiceDispatch, StatusCode, StreamHeader, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use aster_transport_core::reactor::{response_channel, CancelSignal, ResponseReceiver};
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

/// Wait until `n` has a direct address to hand out: a condition wait
/// on the endpoint's address watcher, failing loudly after 30 s rather
/// than letting a later connect fail for an unrelated-looking reason.
async fn wait_for_addr(n: &Node) {
    use aster::native::iroh::Watcher;
    let mut addrs = n.native_endpoint().watch_addr();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while n.addr().direct_addresses.is_empty() {
            addrs.updated().await.expect("endpoint closed");
        }
    })
    .await
    .expect("the node had no direct address after 30 s");
}

async fn within<F: Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

fn header(method: &str) -> StreamHeader {
    StreamHeader {
        service: "Admin".into(),
        method: method.into(),
        version: 1,
        call_id: 0,
        deadline: 0,
        serialization_mode: SerializationMode::Xlang.as_i8(),
        metadata_keys: vec![],
        metadata_values: vec![],
        session_id: 0,
    }
}

/// `restart` requires the `operator` role; `ping` is open.
struct AdminService;

#[async_trait::async_trait]
impl ServiceDispatch for AdminService {
    fn name(&self) -> &str {
        "Admin"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["ping", "restart"]
    }
    fn method_requires(&self, method: &str) -> Option<CapabilityRequirement> {
        match method {
            "restart" => Some(require_role("operator")),
            _ => None,
        }
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let _ = call.recv_request().await;
        // Echo back the caller's role so a passing call is observable.
        let role = call
            .attributes()
            .get("aster.role")
            .cloned()
            .unwrap_or_default();
        let _ = call.respond(role.into_bytes(), &RpcStatus::ok()).await;
    }
}

#[tokio::test]
async fn gate3_capability_check() {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    let attrs = AttributeStore::new();
    let _h = Server::new(&server)
        .register(AdminService)
        .attributes(attrs.clone())
        .serve();

    let conn: RpcConnection = within(client.rpc_connect(&server.id())).await.unwrap();

    // Open method works for anyone.
    let resp = within(conn.unary(&header("ping"), vec![1])).await.unwrap();
    assert_eq!(resp, Vec::<u8>::new(), "no role injected yet");

    // Gated method is denied before the role is injected.
    let err = within(conn.unary(&header("restart"), vec![1]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::PermissionDenied.as_i32()),
        "expected PERMISSION_DENIED, got {err:?}"
    );

    // Application injects the operator role for this peer (e.g. after verifying
    // its attestation chain at admission).
    attrs.set_role(client.id().as_str(), "operator").unwrap();

    // Now the gated method passes and the handler sees the role.
    let resp = within(conn.unary(&header("restart"), vec![1]))
        .await
        .unwrap();
    assert_eq!(resp, b"operator".to_vec());

    // Revoking the role re-closes the gate.
    attrs.remove(client.id().as_str());
    let err = within(conn.unary(&header("restart"), vec![1]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::PermissionDenied.as_i32()),
        "expected PERMISSION_DENIED after revoke, got {err:?}"
    );

    client.shutdown().await;
    server.shutdown().await;
}

struct FixedAuth {
    principal: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Authenticator for FixedAuth {
    async fn authenticate(&self, _ctx: &AuthContext) -> Result<AuthOutcome, RpcStatus> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AuthOutcome {
            principal: Some(self.principal.into()),
            attributes: [("aster.role".into(), "operator".into())].into(),
        })
    }
}

struct ProbeService {
    name: &'static str,
    iroh_only: bool,
}

#[async_trait::async_trait]
impl ServiceDispatch for ProbeService {
    fn name(&self) -> &str {
        self.name
    }
    fn version(&self) -> i32 {
        1
    }
    fn iroh_only(&self) -> bool {
        self.iroh_only
    }
    async fn dispatch(&self, _method: &str, call: Call) {
        let reply = format!("{}:{}", call.peer(), call.provenance().is_authenticated());
        let _ = call.respond(reply.into_bytes(), &RpcStatus::ok()).await;
    }
}

fn call_parts(h: &StreamHeader, provenance: PeerProvenance) -> (CallParts, ResponseReceiver) {
    let (response_sender, response_receiver) = response_channel();
    let (_, request_receiver) = mpsc::channel::<RequestFrame>(1);
    (
        CallParts::new(
            "transport-peer".into(),
            provenance,
            aster::rpc::encode_stream_header(h).unwrap(),
            vec![1],
            FLAG_END_STREAM,
            response_sender,
            request_receiver,
            Arc::new(AtomicBool::new(false)),
        ),
        response_receiver,
    )
}

fn response(mut rx: ResponseReceiver) -> (Vec<u8>, RpcStatus) {
    let (mut data, mut status) = (Vec::new(), None);
    while let Ok(item) = rx.try_recv() {
        let (frame, _lease) = item.into_parts();
        let bytes = match frame {
            OutgoingFrame::Frame(b)
            | OutgoingFrame::Trailer(b)
            | OutgoingFrame::CompleteUnary(b) => b,
        };
        let mut rest = bytes.as_slice();
        while !rest.is_empty() {
            let (payload, flags, consumed) = decode_frame(rest).unwrap();
            if flags & FLAG_TRAILER != 0 {
                status = Some(aster::rpc::codec::decode_rpc_status(&payload).unwrap());
            } else {
                data.extend(payload);
            }
            rest = &rest[consumed..];
        }
    }
    (data, status.expect("terminal trailer"))
}

async fn local_call(
    d: &Dispatcher,
    h: &StreamHeader,
    provenance: PeerProvenance,
) -> (Vec<u8>, RpcStatus) {
    let (parts, rx) = call_parts(h, provenance);
    within(d.dispatch_parts(parts)).await;
    response(rx)
}

#[tokio::test]
async fn listener_auth_clone_keeps_iroh_policy_and_transport_provenance() {
    let node = Node::start(cfg()).await.unwrap();
    let base_calls = Arc::new(AtomicUsize::new(0));
    let listener_calls = Arc::new(AtomicUsize::new(0));
    let base = Server::new(&node)
        .register(ProbeService {
            name: "Probe",
            iroh_only: false,
        })
        .register(ProbeService {
            name: "IrohOnly",
            iroh_only: true,
        })
        .authenticator(FixedAuth {
            principal: "iroh-policy",
            calls: base_calls.clone(),
        })
        .dispatcher();
    let listener = base.with_authenticator(Arc::new(FixedAuth {
        principal: "http-principal",
        calls: listener_calls.clone(),
    }));
    let mut h = header("probe");
    h.service = "Probe".into();
    assert_eq!(
        local_call(&listener, &h, PeerProvenance::Other).await.0,
        b"http-principal:false"
    );
    assert_eq!(
        local_call(&base, &h, PeerProvenance::IrohAuthenticated)
            .await
            .0,
        b"iroh-policy:true"
    );

    // A credential principal never upgrades HTTP to an Iroh identity, and
    // the Iroh-only gate rejects before even running listener auth.
    h.service = "IrohOnly".into();
    assert_eq!(
        local_call(&listener, &h, PeerProvenance::Other)
            .await
            .1
            .code,
        StatusCode::NotFound.as_i32()
    );
    assert_eq!(listener_calls.load(Ordering::SeqCst), 1);
    assert_eq!(base_calls.load(Ordering::SeqCst), 1);
    node.shutdown().await;
}

struct SessionCounter(AtomicUsize);

#[async_trait::async_trait]
impl ServiceDispatch for SessionCounter {
    fn name(&self) -> &str {
        "Counter"
    }
    fn version(&self) -> i32 {
        1
    }
    fn method_requires(&self, _method: &str) -> Option<CapabilityRequirement> {
        Some(require_role("operator"))
    }
    async fn dispatch(&self, _method: &str, call: Call) {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = call.respond(vec![n as u8], &RpcStatus::ok()).await;
    }
}

#[tokio::test]
async fn listener_clones_share_sessions_scoped_to_authenticated_principal() {
    let node = Node::start(cfg()).await.unwrap();
    let base = Server::new(&node)
        .register_session(|| SessionCounter(AtomicUsize::new(0)))
        .dispatcher();
    let auth = |principal| {
        Arc::new(FixedAuth {
            principal,
            calls: Arc::default(),
        }) as Arc<dyn Authenticator>
    };
    let alice = base.with_authenticator(auth("alice"));
    let alice_other_listener = base.with_authenticator(auth("alice"));
    let bob = base.with_authenticator(auth("bob"));
    let mut h = header("increment");
    h.service = "Counter".into();
    h.metadata_keys = vec!["aster-session-id".into()];
    h.metadata_values = vec!["same-session".into()];
    assert_eq!(
        local_call(&alice, &h, PeerProvenance::Other).await.0,
        vec![1]
    );
    assert_eq!(
        local_call(&alice_other_listener, &h, PeerProvenance::Other)
            .await
            .0,
        vec![2]
    );
    assert_eq!(local_call(&bob, &h, PeerProvenance::Other).await.0, vec![1]);
    assert_eq!(
        local_call(&alice, &h, PeerProvenance::Other).await.0,
        vec![3]
    );
    node.shutdown().await;
}

struct PendingAuth {
    entered: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

struct MarkDropped(Arc<AtomicBool>);
impl Drop for MarkDropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[async_trait::async_trait]
impl Authenticator for PendingAuth {
    async fn authenticate(&self, _ctx: &AuthContext) -> Result<AuthOutcome, RpcStatus> {
        let _guard = MarkDropped(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn routed_deadline_drops_pending_authentication() {
    let node = Node::start(cfg()).await.unwrap();
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let d = Server::new(&node)
        .register(AdminService)
        .handler_timeout(Some(Duration::from_millis(50)))
        .authenticator(PendingAuth {
            entered: entered.clone(),
            dropped: dropped.clone(),
        })
        .dispatcher();
    let (parts, rx) = call_parts(&header("ping"), PeerProvenance::Other);
    let cancelled = parts.cancelled.clone();
    let task = tokio::spawn(async move { d.dispatch_parts(parts).await });
    within(entered.notified()).await;
    within(task).await.unwrap();
    assert!(dropped.load(Ordering::Acquire));
    assert!(cancelled.load(Ordering::Acquire));
    let (data, status) = response(rx);
    assert!(data.is_empty());
    assert_eq!(status.code, StatusCode::DeadlineExceeded.as_i32());
    node.shutdown().await;
}

#[tokio::test]
async fn peer_cancellation_drops_pending_authentication_without_trailer() {
    let node = Node::start(cfg()).await.unwrap();
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let d = Server::new(&node)
        .register(AdminService)
        .handler_timeout(None)
        .authenticator(PendingAuth {
            entered: entered.clone(),
            dropped: dropped.clone(),
        })
        .dispatcher();
    let (mut parts, mut rx) = call_parts(&header("ping"), PeerProvenance::Other);
    let cancelled = parts.cancelled.clone();
    let signal = Arc::new(CancelSignal::new());
    parts.cancel_notify = Some(signal.clone());
    let task = tokio::spawn(async move { d.dispatch_parts(parts).await });
    within(entered.notified()).await;
    cancelled.store(true, Ordering::Release);
    signal.notify_waiters();
    within(task).await.unwrap();
    assert!(dropped.load(Ordering::Acquire));
    assert!(
        rx.try_recv().is_err(),
        "cancelled peer must receive no trailer"
    );
    node.shutdown().await;
}

#[tokio::test]
async fn expired_or_cancelled_route_does_not_begin_authentication() {
    let node = Node::start(cfg()).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let d = Server::new(&node)
        .register(AdminService)
        .handler_timeout(Some(Duration::from_millis(20)))
        .authenticator(FixedAuth {
            principal: "alice",
            calls: calls.clone(),
        })
        .dispatcher();
    let (parts, rx) = call_parts(&header("ping"), PeerProvenance::Other);
    let route = d.route(&parts);
    tokio::time::sleep(Duration::from_millis(30)).await;
    within(d.complete(route, parts)).await;
    assert_eq!(response(rx).1.code, StatusCode::DeadlineExceeded.as_i32());
    let (parts, mut rx) = call_parts(&header("ping"), PeerProvenance::Other);
    parts.cancelled.store(true, Ordering::Release);
    within(d.dispatch_parts(parts)).await;
    assert!(rx.try_recv().is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    node.shutdown().await;
}

#[cfg(feature = "binding-support")]
#[tokio::test]
async fn foreign_authentication_gates_session_creation_and_obeys_routed_deadline() {
    use aster::binding_support::upcall::auth::ForeignAuthenticator;
    use aster::binding_support::upcall::codec::{self, RpcAuthResponse};
    use aster::binding_support::upcall::{
        CancelReason, UpcallKind, UpcallQueue, UpcallStatus, Upcalls,
    };

    let node = Node::start(cfg()).await.unwrap();
    let queue = UpcallQueue::new(8);
    let upcalls = Upcalls::new(queue.clone());
    let created = Arc::new(AtomicUsize::new(0));
    let factory_count = created.clone();
    let base = Server::new(&node)
        .handler_timeout(Some(Duration::from_millis(100)))
        .register_session(move || {
            factory_count.fetch_add(1, Ordering::SeqCst);
            SessionCounter(AtomicUsize::new(0))
        })
        .dispatcher();
    let listener = base.with_authenticator(Arc::new(ForeignAuthenticator::new(upcalls.clone())));
    // register_session constructs one metadata probe before any calls arrive.
    let probes = created.load(Ordering::SeqCst);
    let mut h = header("increment");
    h.service = "Counter".into();
    h.session_id = 7;
    h.metadata_keys = vec!["authorization".into()];
    h.metadata_values = vec!["Bearer secret".into()];

    for (answer, expected) in [
        (RpcAuthResponse::Rejected, StatusCode::Unauthenticated),
        (
            RpcAuthResponse::Authenticated {
                principal: "user/alice".into(),
                attributes: Default::default(),
            },
            StatusCode::PermissionDenied,
        ),
        (
            RpcAuthResponse::Authenticated {
                principal: "user/alice".into(),
                attributes: [("aster.role".into(), "operator".into())].into(),
            },
            StatusCode::Ok,
        ),
    ] {
        let d = listener.clone();
        let (parts, rx) = call_parts(&h, PeerProvenance::Other);
        let task = tokio::spawn(async move { d.dispatch_parts(parts).await });
        within(queue.ready()).await;
        let mut event = None;
        queue.drain(1, |value| event = Some(value));
        let event = event.unwrap();
        assert_eq!(event.kind, UpcallKind::RpcAuthenticate);
        let request = codec::decode_rpc_auth_request(&event.payload).unwrap();
        assert_eq!(request.service, "Counter");
        assert_eq!(request.session_id, 7);
        assert_eq!(request.metadata["authorization"], "Bearer secret");
        upcalls
            .respond(
                event.invocation_id,
                UpcallStatus::Ok,
                codec::encode_rpc_auth_response(&answer).unwrap(),
            )
            .unwrap();
        within(task).await.unwrap();
        let (data, status) = response(rx);
        assert_eq!(status.code, expected.as_i32());
        if expected == StatusCode::Ok {
            assert_eq!(data, vec![1]);
            assert_eq!(created.load(Ordering::SeqCst), probes + 1);
        } else {
            assert!(data.is_empty());
            assert_eq!(
                created.load(Ordering::SeqCst),
                probes,
                "refused auth created a session"
            );
        }
        assert_eq!(upcalls.pending_count(), 0);
    }

    // A listener deadline drops the adapter's pending upcall, emits its
    // cancellation and never creates the next session from a late answer.
    h.session_id = 8;
    let (parts, rx) = call_parts(&h, PeerProvenance::Other);
    let task = tokio::spawn(async move { listener.dispatch_parts(parts).await });
    within(queue.ready()).await;
    let mut event = None;
    queue.drain(1, |value| event = Some(value));
    let event = event.unwrap();
    within(task).await.unwrap();
    assert_eq!(response(rx).1.code, StatusCode::DeadlineExceeded.as_i32());
    within(queue.ready()).await;
    let mut cancel = None;
    queue.drain(1, |value| cancel = Some(value));
    let cancel = cancel.unwrap();
    assert_eq!(cancel.kind, UpcallKind::Cancel);
    assert_eq!(cancel.invocation_id, event.invocation_id);
    assert_eq!(cancel.cancel_reason(), Some(CancelReason::Abandoned));
    assert_eq!(upcalls.pending_count(), 0);
    assert_eq!(created.load(Ordering::SeqCst), probes + 1);
    assert!(upcalls
        .respond(
            event.invocation_id,
            UpcallStatus::Ok,
            codec::encode_rpc_auth_response(&RpcAuthResponse::Rejected).unwrap()
        )
        .is_err());
    node.shutdown().await;
}
