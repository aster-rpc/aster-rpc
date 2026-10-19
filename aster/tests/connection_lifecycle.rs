#![cfg(all(
    feature = "rpc",
    feature = "binding-support",
    feature = "reactor-test-hooks"
))]
//! Close-versus-dispatch lifecycle regressions: a call fully read on a stream
//! whose task is descheduled until after the connection's close must not leave
//! per-connection state (foreign admission, a session) that no later event
//! reaps. Run with:
//! cargo test -p aster --features rpc,binding-support,reactor-test-hooks --test connection_lifecycle

use std::time::Duration;

use aster::binding_support::dispatch::{
    CancelReason, DispatchAdmission, DispatchEvent, DispatchSink, RingConsumer, RingSink,
};
use aster::rpc::{
    AsterServer, DynamicService, MethodDesc, MethodPattern, RpcStatus, ScopeKind, StreamHeader,
    TypeRef,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{encode_frame, FLAG_END_STREAM, FLAG_HEADER};
use aster_transport_core::reactor::test_hooks::{self, CallEnqueueGate};

const WAIT: Duration = Duration::from_secs(15);

/// The enqueue gate is process-wide; tests in this binary take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn config() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn within<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("connection lifecycle operation timed out")
}

async fn connect(server: &AsterServer) -> (Node, aster::rpc::RpcConnection) {
    let client = Node::start(config()).await.unwrap();
    for node in [server.node(), &client] {
        for _ in 0..50 {
            if !node.addr().direct_addresses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    client.add_peer(server.node()).unwrap();
    server.node().add_peer(&client).unwrap();
    let connection = within(client.rpc_connect(&server.id())).await.unwrap();
    (client, connection)
}

/// A server whose foreign sink admits one call, one connection and one
/// session at a time, so a single leaked admission refuses the next peer.
async fn server(scope: ScopeKind) -> (AsterServer, std::sync::Arc<RingSink>, RingConsumer) {
    let (sink, consumer) = RingSink::new_with_limit(1, 1);
    let mut service = DynamicService::new("test.Lifecycle", 1);
    service.scope = scope;
    service.methods = vec![MethodDesc::new(
        "echo",
        MethodPattern::Unary,
        TypeRef::new("a1".repeat(32)),
        TypeRef::new("b2".repeat(32)),
    )];
    let server = AsterServer::builder()
        .config(config())
        .dispatch_sink(sink.clone())
        .dynamic_service(service)
        .unwrap()
        .start()
        .await
        .unwrap();
    (server, sink, consumer)
}

fn header(scope: ScopeKind) -> StreamHeader {
    StreamHeader {
        service: "test.Lifecycle".into(),
        method: "echo".into(),
        version: 1,
        session_id: if scope == ScopeKind::Session { 1 } else { 0 },
        ..Default::default()
    }
}

/// Write one complete unary request on a fresh multiplexed stream, bypassing
/// the client so nothing waits for the response.
async fn send_request(connection: &aster::rpc::RpcConnection, header: &StreamHeader) {
    let (send, _recv) = connection
        .core_connection()
        .open_streaming_substream()
        .await
        .unwrap();
    let encoded_header = aster::rpc::encode_stream_header(header).unwrap();
    let mut request = encode_frame(&encoded_header, FLAG_HEADER).unwrap();
    request.extend(encode_frame(b"departing", FLAG_END_STREAM).unwrap());
    send.write_all(request).await.unwrap();
}

/// After the gated call has settled: suppressing it is correct; delivering it
/// is acceptable only if it is cancelled and everything it admitted is
/// reaped. Then a fresh peer must be served, which fails with
/// RESOURCE_EXHAUSTED if the late call resurrected an admission.
async fn assert_late_call_left_nothing(
    server: AsterServer,
    sink: &RingSink,
    consumer: &mut RingConsumer,
    gate: &mut CallEnqueueGate,
    connection_id: u64,
    scope: ScopeKind,
    departing_client: Node,
) {
    let published = within(gate.settled()).await;
    let mut late_call = None;
    let mut cancelled = !published;
    while !cancelled {
        within(consumer.ready()).await;
        consumer.drain(8, |event| match event {
            DispatchEvent::Call(call) => {
                assert_eq!(call.connection_id, connection_id);
                late_call = Some((call.call_id, call.session_key));
                call.handle.detach();
            }
            DispatchEvent::Cancelled { call_id, reason } => {
                assert_eq!(Some(call_id), late_call.map(|(id, _)| id));
                assert_eq!(reason, CancelReason::Peer);
                cancelled = true;
            }
            other => panic!("unexpected departing connection event: {other:?}"),
        });
    }
    if let (Some((_, session_key)), ScopeKind::Session) = (late_call, scope) {
        // A session created for a closed connection is reaped by nothing.
        let mut reaped = false;
        while !reaped {
            tokio::time::timeout(WAIT, consumer.ready())
                .await
                .expect("a session created after its connection closed was never reaped");
            consumer.drain(8, |event| match event {
                DispatchEvent::SessionClosed { session_key: key } => {
                    assert_eq!(key, session_key);
                    reaped = true;
                }
                other => panic!("unexpected departing connection event: {other:?}"),
            });
        }
    }
    assert!(
        server.call_table().is_empty(),
        "cancelled call was not reaped"
    );
    assert!(
        sink.try_admit(DispatchAdmission::Call).is_some(),
        "the drained cancellation must release call capacity"
    );

    let (fresh_client, fresh) = connect(&server).await;
    let fresh_header = header(scope);
    let unary = fresh.unary(&fresh_header, b"healthy".to_vec());
    tokio::pin!(unary);
    let result = within(async {
        loop {
            tokio::select! {
                result = &mut unary => break result,
                () = consumer.ready() => {
                    consumer.drain(8, |event| match event {
                        DispatchEvent::Call(call) => {
                            call.handle.try_respond(&call.request, &RpcStatus::ok()).unwrap();
                        }
                        other => panic!("unexpected fresh connection event: {other:?}"),
                    });
                }
            }
        }
    })
    .await;
    fresh_client.shutdown().await;
    departing_client.shutdown().await;
    server.shutdown().await;
    assert_eq!(
        result.expect("a closed connection leaked foreign admission capacity"),
        b"healthy"
    );
}

/// The call pauses before publication until its connection's close event is
/// enqueued, then resumes.
async fn descheduled_past_close(scope: ScopeKind) {
    let _serial = SERIAL.lock().await;
    let (server, sink, mut consumer) = server(scope).await;
    let (departing_client, departing) = connect(&server).await;
    let mut gate = test_hooks::pause_next_call_enqueue(departing_client.id().to_string());
    send_request(&departing, &header(scope)).await;

    let connection_id = within(gate.paused()).await;
    departing
        .core_connection()
        .close(0, b"finished".to_vec())
        .unwrap();
    within(gate.closed_enqueued()).await;
    // Close is already in the event queue, so the serve loop consumes it before
    // anything this task publishes, irrespective of runtime scheduling.
    gate.resume();
    assert_late_call_left_nothing(
        server,
        &sink,
        &mut consumer,
        &mut gate,
        connection_id,
        scope,
        departing_client,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_connection_with_a_descheduled_call_releases_foreign_capacity() {
    descheduled_past_close(ScopeKind::Shared).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_connection_with_a_descheduled_session_call_leaves_no_session() {
    descheduled_past_close(ScopeKind::Session).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_blocked_on_a_full_queue_is_not_published_after_close() {
    let _serial = SERIAL.lock().await;
    let scope = ScopeKind::Shared;
    let (server, sink, mut consumer) = server(scope).await;
    let (departing_client, departing) = connect(&server).await;
    let mut gate =
        test_hooks::pause_next_call_enqueue_with_full_queue(departing_client.id().to_string());
    send_request(&departing, &header(scope)).await;

    assert!(
        within(gate.filled()).await > 0,
        "the event queue had no free slot to hold"
    );
    let connection_id = within(gate.paused()).await;
    departing
        .core_connection()
        .close(0, b"finished".to_vec())
        .unwrap();
    // The close is marked and its event now waits for a slot in the full
    // queue, ahead of the call's publication.
    within(gate.close_sending()).await;
    gate.resume();
    gate.release_queue();
    // Neither side holds anything across the wait for capacity: both finish.
    within(gate.closed_enqueued()).await;
    assert_late_call_left_nothing(
        server,
        &sink,
        &mut consumer,
        &mut gate,
        connection_id,
        scope,
        departing_client,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_waiting_on_a_full_queue_ends_when_the_server_shuts_down() {
    let _serial = SERIAL.lock().await;
    let scope = ScopeKind::Shared;
    let (server, _sink, _consumer) = server(scope).await;
    let (client, connection) = connect(&server).await;
    let mut gate = test_hooks::pause_next_call_enqueue_with_full_queue(client.id().to_string());
    send_request(&connection, &header(scope)).await;

    assert!(
        within(gate.filled()).await > 0,
        "the event queue had no free slot to hold"
    );
    within(gate.paused()).await;
    // The call now waits for a slot in the full queue; the queue is never
    // released. Shutdown must neither wait on that call nor let it publish.
    gate.resume();
    assert!(
        // short-wait-ok: the call must still be waiting for a queue slot; expiry is the pass
        tokio::time::timeout(Duration::from_millis(300), gate.settled())
            .await
            .is_err(),
        "the call should be waiting for a queue slot"
    );
    within(server.shutdown()).await;
    assert!(
        !within(gate.settled()).await,
        "a call was published into a server that had shut down"
    );
    client.shutdown().await;
}
