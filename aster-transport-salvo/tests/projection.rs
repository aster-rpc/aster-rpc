//! Browser JSON/NDJSON projections over HTTP. A hand-written `Projection`
//! transcodes JSON⇄Fory at the edge; the dispatcher runs ordinary Fory. Proves
//! the gateway tier (unary→JSON, server-stream→NDJSON, negotiation status
//! codes) end-to-end, ahead of the generated-adapter macro.

use std::sync::Arc;

use aster::rpc::{
    new_payload_fory, AsterType, Call, Fory, MethodPattern, Projection, ProjectionRegistry,
    RpcStatus, Server, ServiceDispatch,
};
use aster::{AsterConfig, Node, RelayMode};
use fory_derive::ForyStruct;
use salvo::http::StatusCode as HttpStatus;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};
use serde::{Deserialize, Serialize};

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "calc/AddReq")]
struct AddReq {
    a: i32,
    b: i32,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "calc/AddResp")]
struct AddResp {
    sum: i32,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "calc/CountReq")]
struct CountReq {
    n: i32,
}

#[derive(
    ForyStruct, aster::AsterType, Serialize, Deserialize, Debug, Default, Clone, PartialEq,
)]
#[aster(wire = "calc/NumResp")]
struct NumResp {
    v: i32,
}

fn build_fory() -> Fory {
    let mut f = new_payload_fory();
    macro_rules! reg {
        ($($t:ty),*) => {$({
            let td = <$t as AsterType>::aster_type_def();
            f.register_by_name::<$t>(&td.package, &td.name).unwrap();
        })*};
    }
    reg!(AddReq, AddResp, CountReq, NumResp);
    f
}

/// Raw Fory service.
struct Calc {
    fory: Arc<Fory>,
}

#[aster::rpc::async_trait]
impl ServiceDispatch for Calc {
    fn name(&self) -> &str {
        "Calc"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["add", "count"]
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        match method {
            "add" => {
                let raw = call.recv_request().await.unwrap_or_default();
                let req: AddReq = self.fory.deserialize(&raw).unwrap();
                let bytes = self
                    .fory
                    .serialize(&AddResp { sum: req.a + req.b })
                    .unwrap();
                let _ = call.respond(bytes, &RpcStatus::ok());
            }
            "count" => {
                let raw = call.recv_request().await.unwrap_or_default();
                let req: CountReq = self.fory.deserialize(&raw).unwrap();
                for v in 0..req.n {
                    let _ = call.send(self.fory.serialize(&NumResp { v }).unwrap());
                }
                let _ = call.finish(&RpcStatus::ok());
            }
            _ => {}
        }
    }
}

/// Hand-written JSON⇄Fory projection (the macro will generate this later).
struct CalcProjection {
    fory: Arc<Fory>,
}

impl Projection for CalcProjection {
    fn service(&self) -> &str {
        "Calc"
    }
    fn pattern(&self, method: &str) -> Option<MethodPattern> {
        match method {
            "add" => Some(MethodPattern::Unary),
            "count" => Some(MethodPattern::ServerStream),
            _ => None,
        }
    }
    fn request_to_fory(&self, method: &str, _media: &str, body: &[u8]) -> Result<Vec<u8>, String> {
        let to_err = |e: String| e;
        match method {
            "add" => {
                let v: AddReq = serde_json::from_slice(body).map_err(|e| e.to_string())?;
                self.fory.serialize(&v).map_err(|e| to_err(e.to_string()))
            }
            "count" => {
                let v: CountReq = serde_json::from_slice(body).map_err(|e| e.to_string())?;
                self.fory.serialize(&v).map_err(|e| e.to_string())
            }
            _ => Err("unknown method".into()),
        }
    }
    fn response_from_fory(
        &self,
        method: &str,
        _media: &str,
        fory: &[u8],
    ) -> Result<Vec<u8>, String> {
        match method {
            "add" => {
                let v: AddResp = self.fory.deserialize(fory).map_err(|e| e.to_string())?;
                serde_json::to_vec(&v).map_err(|e| e.to_string())
            }
            "count" => {
                let v: NumResp = self.fory.deserialize(fory).map_err(|e| e.to_string())?;
                serde_json::to_vec(&v).map_err(|e| e.to_string())
            }
            _ => Err("unknown method".into()),
        }
    }
}

async fn service() -> (Node, Service) {
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();
    let fory = Arc::new(build_fory());
    let dispatcher = Server::new(&node)
        .register(Calc { fory: fory.clone() })
        .dispatcher();
    let projections = ProjectionRegistry::new().register(CalcProjection { fory });
    let svc = Service::new(aster_transport_salvo::router_with(dispatcher, projections));
    (node, svc)
}

#[tokio::test]
async fn unary_json_projection() {
    let (node, svc) = service().await;

    let mut res = TestClient::post("http://localhost/aster/Calc/add")
        .add_header("content-type", "application/json", true)
        .add_header("accept", "application/json", true)
        .body(r#"{"a":2,"b":3}"#)
        .send(&svc)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::OK));
    let bytes = res.take_bytes(None).await.unwrap();
    let resp: AddResp = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(resp.sum, 5);

    node.shutdown().await;
}

#[tokio::test]
async fn server_stream_ndjson_projection() {
    let (node, svc) = service().await;

    let mut res = TestClient::post("http://localhost/aster/Calc/count")
        .add_header("content-type", "application/json", true)
        .add_header("accept", "application/x-ndjson", true)
        .body(r#"{"n":3}"#)
        .send(&svc)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::OK));
    let bytes = res.take_bytes(None).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let nums: Vec<NumResp> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        nums,
        vec![NumResp { v: 0 }, NumResp { v: 1 }, NumResp { v: 2 }]
    );

    node.shutdown().await;
}

#[tokio::test]
async fn unsupported_media_and_method() {
    let (node, svc) = service().await;

    // Unknown service → no projection → 415.
    let res = TestClient::post("http://localhost/aster/Nope/x")
        .add_header("content-type", "application/json", true)
        .body("{}")
        .send(&svc)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::UNSUPPORTED_MEDIA_TYPE));

    // Unknown method on a projected service → 404.
    let res = TestClient::post("http://localhost/aster/Calc/ghost")
        .add_header("content-type", "application/json", true)
        .body("{}")
        .send(&svc)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::NOT_FOUND));

    node.shutdown().await;
}
