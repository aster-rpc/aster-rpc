//! Mission Control over HTTP — typed, end-to-end. Drives the real
//! `MissionControl` service through the Salvo transport with Fory-encoded
//! payloads (what an HTTP client binding does), covering all four call patterns.

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{new_payload_fory, AsterType, Fory, RpcStatus, Server};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, encode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use mission_control::*;
use salvo::http::StatusCode as HttpStatus;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};

/// Register a payload type by its cross-binding wire name (package + name),
/// exactly as the generated server/client do.
macro_rules! register {
    ($f:expr, $($t:ty),* $(,)?) => {
        $({
            let td = <$t as AsterType>::aster_type_def();
            $f.register_by_name::<$t>(&td.package, &td.name).unwrap();
        })*
    };
}

fn payload_fory() -> Fory {
    let mut f = new_payload_fory();
    register!(
        f,
        StatusRequest,
        StatusResponse,
        LogEntry,
        SubmitLogResult,
        TailRequest,
        MetricPoint,
        IngestResult,
        Command,
        CommandResult,
    );
    f
}

/// POST request frames to a method; return (data payloads, trailer status).
async fn call_http(
    service: &Service,
    method: &str,
    req_frames: Vec<Vec<u8>>,
) -> (Vec<Vec<u8>>, RpcStatus) {
    let mut body = Vec::new();
    let n = req_frames.len();
    for (i, payload) in req_frames.into_iter().enumerate() {
        let flags = if i + 1 == n { FLAG_END_STREAM } else { 0 };
        body.extend_from_slice(&encode_frame(&payload, flags).unwrap());
    }
    let mut res = TestClient::post(format!("http://localhost/aster/MissionControl/{method}"))
        .body(body)
        .send(service)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::OK), "method {method}");
    let bytes = res.take_bytes(None).await.unwrap();

    let mut buf = bytes.as_ref();
    let mut data = Vec::new();
    let mut trailer = None;
    while !buf.is_empty() {
        let (payload, flags, consumed) = decode_frame(buf).unwrap();
        if flags & FLAG_TRAILER != 0 {
            trailer = Some(decode_rpc_status(&payload).unwrap());
        } else {
            data.push(payload);
        }
        buf = &buf[consumed..];
    }
    (data, trailer.expect("response missing trailer"))
}

#[tokio::test]
async fn mission_control_over_http() {
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();
    let dispatcher = Server::new(&node)
        .register(MissionControlServer::new(MissionControlImpl))
        .dispatcher();
    let service = Service::new(aster_transport_salvo::router(dispatcher));
    let fory = payload_fory();

    // unary — get_status
    let req = fory
        .serialize(&StatusRequest {
            agent_id: "agent-1".into(),
        })
        .unwrap();
    let (data, status) = call_http(&service, "get_status", vec![req]).await;
    assert_eq!(status.code, 0);
    let resp: StatusResponse = fory.deserialize(&data[0]).unwrap();
    assert_eq!(resp.agent_id, "agent-1");
    assert_eq!(resp.status, "running");
    assert_eq!(resp.uptime_secs, 3600);

    // server-stream — tail_logs → 3 entries
    let req = fory
        .serialize(&TailRequest {
            agent_id: "agent-1".into(),
            level: "info".into(),
        })
        .unwrap();
    let (data, status) = call_http(&service, "tail_logs", vec![req]).await;
    assert_eq!(status.code, 0);
    assert_eq!(data.len(), 3);
    let first: LogEntry = fory.deserialize(&data[0]).unwrap();
    assert_eq!(first.message, "log 0");
    assert_eq!(first.agent_id, "agent-1");

    // client-stream — ingest_metrics (3 points) → accepted 3
    let pts: Vec<Vec<u8>> = (0..3)
        .map(|i| {
            fory.serialize(&MetricPoint {
                name: format!("m{i}"),
                value: i as f64,
                timestamp: 0.0,
            })
            .unwrap()
        })
        .collect();
    let (data, status) = call_http(&service, "ingest_metrics", pts).await;
    assert_eq!(status.code, 0);
    let res: IngestResult = fory.deserialize(&data[0]).unwrap();
    assert_eq!(res.accepted, 3);
    assert_eq!(res.dropped, 0);

    // bidi — run_command (2 commands) → 2 results
    let cmds: Vec<Vec<u8>> = ["echo a", "echo b"]
        .iter()
        .map(|c| {
            fory.serialize(&Command {
                command: c.to_string(),
            })
            .unwrap()
        })
        .collect();
    let (data, status) = call_http(&service, "run_command", cmds).await;
    assert_eq!(status.code, 0);
    assert_eq!(data.len(), 2);
    let r0: CommandResult = fory.deserialize(&data[0]).unwrap();
    assert_eq!(r0.stdout, "ran: echo a");
    assert_eq!(r0.exit_code, 0);

    node.shutdown().await;
}
