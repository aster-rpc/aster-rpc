#![cfg(all(
    feature = "rpc",
    feature = "binding-support",
    feature = "reactor-test-hooks"
))]
//! Draft close-versus-dispatch lifecycle regression; awaiting Claude validation.
//! Not compiled or executed. The current late-event expectation must be revised
//! to also accept a correct implementation that suppresses the late call.
//! This ignore is not a qualification waiver. Run explicitly after review with:
//! cargo test -p aster --features rpc,binding-support,reactor-test-hooks --test connection_lifecycle -- --ignored

use std::time::Duration;

use aster::binding_support::dispatch::{
    CancelReason, DispatchAdmission, DispatchEvent, DispatchSink, RingSink,
};
use aster::rpc::{
    AsterServer, DynamicService, MethodDesc, MethodPattern, RpcStatus, StreamHeader, TypeRef,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{encode_frame, FLAG_END_STREAM, FLAG_HEADER};
use aster_transport_core::reactor::test_hooks;

const WAIT: Duration = Duration::from_secs(15);

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "draft: awaiting Claude validation and correction of late-event expectation"]
async fn closed_connection_with_a_descheduled_call_releases_foreign_capacity() {
    let (sink, mut consumer) = RingSink::new_with_limit(1, 1);
    let mut service = DynamicService::new("test.Lifecycle", 1);
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
    let header = StreamHeader {
        service: "test.Lifecycle".into(),
        method: "echo".into(),
        version: 1,
        ..Default::default()
    };
    let (departing_client, departing) = connect(&server).await;
    let mut gate = test_hooks::pause_next_call_enqueue(departing_client.id());
    let (send, _recv) = departing
        .core_connection()
        .open_streaming_substream()
        .await
        .unwrap();
    let encoded_header = aster::rpc::encode_stream_header(&header).unwrap();
    let mut request = encode_frame(&encoded_header, FLAG_HEADER).unwrap();
    request.extend(encode_frame(b"departing", FLAG_END_STREAM).unwrap());
    send.write_all(request).await.unwrap();

    // The complete request is now owned by a stream task paused immediately
    // before Call publication. Closing is ordinary connection teardown.
    let connection_id = within(gate.paused()).await;
    departing.core_connection().close(0, b"finished".to_vec());
    within(gate.closed_enqueued()).await;
    // Close is already in the event queue. The serve loop consumes it before
    // this task can publish Call, irrespective of runtime scheduling.
    gate.resume();

    let mut late_call_id = None;
    let mut cancelled = false;
    while !cancelled {
        within(consumer.ready()).await;
        consumer.drain(8, |event| match event {
            DispatchEvent::Call(call) => {
                assert_eq!(call.connection_id, connection_id);
                late_call_id = Some(call.handle.detach());
            }
            DispatchEvent::Cancelled { call_id, reason } => {
                assert_eq!(Some(call_id), late_call_id);
                assert_eq!(reason, CancelReason::Peer);
                cancelled = true;
            }
            other => panic!("unexpected departing connection event: {other:?}"),
        });
    }
    assert!(
        server.call_table().is_empty(),
        "cancelled call was not reaped"
    );
    assert!(
        sink.try_admit(DispatchAdmission::Call).is_some(),
        "the drained cancellation must release call capacity"
    );

    // A separate peer must still be able to use the server. This fails with
    // RESOURCE_EXHAUSTED if the late Call resurrected connection admission.
    let (fresh_client, fresh) = connect(&server).await;
    let unary = fresh.unary(&header, b"healthy".to_vec());
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
        result.expect("closed connection leaked foreign admission capacity"),
        b"healthy"
    );
}
