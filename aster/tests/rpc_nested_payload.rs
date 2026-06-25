#![cfg(feature = "rpc")]
//! Regression: nested struct payloads register transitively. The
//! `#[aster::service]` macro names only each method's top-level request/response
//! type, but `#[derive(AsterType)]` now walks every field's element/value/key
//! type into the Fory runtime (`WireField::register_payload`). Before this, a
//! payload like `SegmentList { segments: Vec<Segment> }` registered `SegmentList`
//! but not `Segment`, and the resolver failed with "TypeId ... not found".
//!
//! This exercises a deeply nested shape end-to-end over the real transport:
//! `Vec<UserStruct>`, a struct-in-struct, an `Option<UserStruct>`, and a
//! `HashMap<String, UserStruct>` — all reachable only via nesting.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use aster::rpc::{async_trait, RpcConnection, Server, RPC_ALPN};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use tokio::time::timeout;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "md/Segment")]
struct Segment {
    name: String,
    size: i64,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "md/SymbolInfo")]
struct SymbolInfo {
    symbol: String,
    exchange: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "md/RefreshRequest")]
struct RefreshRequest {
    // struct-in-struct (Ref), reachable only transitively
    head: SymbolInfo,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "md/SegmentList")]
struct SegmentList {
    // Vec<UserStruct> — the exact shape that failed before the fix
    segments: Vec<Segment>,
    // Option<UserStruct>
    primary: Option<SymbolInfo>,
    // HashMap<String, UserStruct>
    by_name: HashMap<String, Segment>,
}

#[aster::service(name = "MarketDataStore", version = 1)]
trait MarketDataStore {
    async fn list_segments(&self, req: RefreshRequest) -> Result<SegmentList>;
}

struct StoreImpl;

#[async_trait]
impl MarketDataStore for StoreImpl {
    async fn list_segments(&self, req: RefreshRequest) -> Result<SegmentList> {
        let seg = Segment {
            name: format!("seg-{}", req.head.symbol),
            size: 42,
        };
        let mut by_name = HashMap::new();
        by_name.insert(seg.name.clone(), seg.clone());
        Ok(SegmentList {
            segments: vec![seg],
            primary: Some(req.head),
            by_name,
        })
    }
}

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

async fn within<F: Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

#[tokio::test]
async fn nested_struct_payloads_round_trip() {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client_node = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client_node).await;
    client_node.add_peer(&server).unwrap();
    server.add_peer(&client_node).unwrap();

    let _h = Server::new(&server)
        .register(MarketDataStoreServer::new(StoreImpl))
        .serve();

    let conn: RpcConnection = within(client_node.rpc_connect(&server.id())).await.unwrap();
    let client = MarketDataStoreClient::new(conn);

    let resp = within(client.list_segments(RefreshRequest {
        head: SymbolInfo {
            symbol: "BTC".into(),
            exchange: "x".into(),
        },
    }))
    .await
    .unwrap();

    assert_eq!(resp.segments.len(), 1);
    assert_eq!(resp.segments[0].name, "seg-BTC");
    assert_eq!(resp.segments[0].size, 42);
    assert_eq!(resp.primary.as_ref().unwrap().symbol, "BTC");
    assert_eq!(resp.by_name.get("seg-BTC").unwrap().size, 42);

    client_node.shutdown().await;
    server.shutdown().await;
}
