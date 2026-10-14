#![cfg(all(feature = "rpc", feature = "docs", feature = "blobs"))]
//! Dynamic proxy client (`aster::rpc::DynamicClient`, plan D5) against a typed
//! `#[aster::service]` server: all four call patterns, JSON and `DynValue`
//! payloads, with the contract fetched and verified through the registry.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use aster::rpc::{
    async_trait, fetch_and_verify_contract, publish_contract, AsterType, DynContract, DynValue,
    DynamicClient, RequestStream, ResponseSink, RpcConnection, Server, StatusCode, TypeDef,
    RPC_ALPN,
};
use aster::{AsterConfig, Error, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use serde_json::json;
use tokio::time::timeout;

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "proxy/Item")]
struct Item {
    name: String,
    qty: i64,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "proxy/Order")]
struct Order {
    id: u32,
    items: Vec<Item>,
    note: Option<String>,
    attrs: BTreeMap<String, String>,
    #[fory(bytes)]
    blob: Vec<u8>,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "proxy/Total")]
struct Total {
    orders: i32,
    qty: i64,
    last_note: Option<String>,
}

#[aster::service(name = "Shop", version = 3)]
trait Shop {
    async fn price(&self, req: Order) -> Result<Total>;

    #[rpc(server_stream)]
    async fn explode(&self, req: Order, out: ResponseSink<Item>) -> Result<()>;

    #[rpc(client_stream)]
    async fn tally(&self, reqs: RequestStream<Order>) -> Result<Total>;

    #[rpc(bidi_stream)]
    async fn running(&self, reqs: RequestStream<Order>, out: ResponseSink<Total>) -> Result<()>;
}

struct ShopImpl;

fn qty(o: &Order) -> i64 {
    o.items.iter().map(|i| i.qty).sum()
}

#[async_trait]
impl Shop for ShopImpl {
    async fn price(&self, req: Order) -> Result<Total> {
        if req.id == 0 {
            return Err(Error::rpc(StatusCode::InvalidArgument, "order id 0"));
        }
        Ok(Total {
            orders: 1,
            qty: qty(&req),
            last_note: req.note,
        })
    }

    async fn explode(&self, req: Order, out: ResponseSink<Item>) -> Result<()> {
        for item in req.items {
            out.send(&item).await?;
        }
        Ok(())
    }

    async fn tally(&self, mut reqs: RequestStream<Order>) -> Result<Total> {
        let mut t = Total::default();
        while let Some(o) = reqs.recv().await? {
            t.orders += 1;
            t.qty += qty(&o);
            t.last_note = o.note.or(t.last_note);
        }
        Ok(t)
    }

    async fn running(
        &self,
        mut reqs: RequestStream<Order>,
        out: ResponseSink<Total>,
    ) -> Result<()> {
        let mut t = Total::default();
        while let Some(o) = reqs.recv().await? {
            t.orders += 1;
            t.qty += qty(&o);
            out.send(&t).await?;
        }
        Ok(())
    }
}

fn type_defs() -> Vec<TypeDef> {
    vec![
        Item::aster_type_def(),
        Order::aster_type_def(),
        Total::aster_type_def(),
    ]
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
    timeout(Duration::from_secs(20), f)
        .await
        .expect("rpc operation timed out")
}

/// Server node serving `Shop` with its contract published into a registry doc
/// it owns, plus a client node connected to it.
async fn setup() -> (Node, Node, RpcConnection, DynContract) {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client_node = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client_node).await;
    client_node.add_peer(&server).unwrap();
    server.add_peer(&client_node).unwrap();
    let _h = Server::new(&server)
        .register(ShopServer::new(ShopImpl))
        .serve();

    let doc = server.docs().create().await.unwrap();
    let author = server.docs().default_author().await.unwrap();
    let sc = ShopClient::contract();
    let published = within(publish_contract(
        &doc,
        &server.blobs(),
        &author,
        &sc,
        &type_defs(),
        1_700_000_000_000,
    ))
    .await
    .unwrap();
    let fetched = within(fetch_and_verify_contract(
        &doc,
        &server.blobs(),
        &server.id(),
        "Shop",
        3,
    ))
    .await
    .unwrap();
    assert_eq!(fetched.contract_id, published.contract_id);
    assert_eq!(fetched.type_defs.len(), 3);
    let contract = fetched.dyn_contract().unwrap();
    assert_eq!(contract.contract_id, published.contract_id);

    let conn = within(client_node.rpc_connect(&server.id())).await.unwrap();
    (server, client_node, conn, contract)
}

fn order_json(id: u32, qtys: &[i64], note: Option<&str>) -> serde_json::Value {
    json!({
        "id": id,
        "items": qtys.iter().enumerate().map(|(i, q)| json!({"name": format!("i{i}"), "qty": q})).collect::<Vec<_>>(),
        "note": note,
        "attrs": {"k": "v"},
        "blob": "AQI",
    })
}

#[tokio::test]
async fn proxy_calls_all_four_patterns() {
    let (_s, _c, conn, contract) = setup().await;
    let proxy = DynamicClient::new(conn.clone(), contract.clone());

    // unary, JSON in / out (int64 as strings per the Aster JSON table)
    let total = within(proxy.call_json("price", order_json(7, &[2, 3], Some("rush"))))
        .await
        .unwrap();
    assert_eq!(total, json!({"orders": 1, "qty": "5", "last_note": "rush"}));

    // the typed client sees the same service
    let typed = ShopClient::new(conn.clone());
    let t = within(typed.price(Order {
        id: 1,
        items: vec![Item {
            name: "x".into(),
            qty: 9,
        }],
        ..Order::default()
    }))
    .await
    .unwrap();
    assert_eq!(t.qty, 9);

    // server stream
    let items = within(proxy.call_json("explode", order_json(1, &[1, 2, 3], None)))
        .await
        .unwrap();
    assert_eq!(
        items,
        json!([
            {"name": "i0", "qty": "1"},
            {"name": "i1", "qty": "2"},
            {"name": "i2", "qty": "3"}
        ])
    );

    // client stream (an array of requests)
    let total = within(proxy.call_json(
        "tally",
        json!([
            order_json(1, &[1], Some("a")),
            order_json(2, &[10, 20], None)
        ]),
    ))
    .await
    .unwrap();
    assert_eq!(total, json!({"orders": 2, "qty": "31", "last_note": "a"}));

    // bidi, DynValue in / out through RpcConnection::call_dynamic
    let reqs = vec![
        DynValue::from_json(&order_json(1, &[4], None)),
        DynValue::from_json(&order_json(2, &[6], None)),
    ];
    let out = within(conn.call_dynamic(&contract, "running", reqs))
        .await
        .unwrap();
    let qtys: Vec<DynValue> = out
        .iter()
        .map(|v| v.as_struct().unwrap().get("qty").unwrap().clone())
        .collect();
    assert_eq!(qtys, vec![DynValue::I64(4), DynValue::I64(10)]);

    // A streaming handle, read incrementally.
    let mut stream =
        within(proxy.server_stream("explode", &DynValue::from_json(&order_json(1, &[5], None))))
            .await
            .unwrap();
    let first = within(stream.next()).await.unwrap().unwrap();
    assert_eq!(
        first.as_struct().unwrap().get("qty"),
        Some(&DynValue::I64(5))
    );
    assert!(within(stream.next()).await.is_none());
}

#[tokio::test]
async fn proxy_errors() {
    let (_s, _c, conn, contract) = setup().await;
    let proxy = DynamicClient::new(conn, contract);

    // Server status passes through.
    let err = within(proxy.call_json("price", order_json(0, &[], None)))
        .await
        .unwrap_err();
    assert_eq!(err.rpc_code(), Some(StatusCode::InvalidArgument), "{err}");

    // Codec errors are caught before the call.
    let err = within(proxy.call_json("price", json!({"id": -1})))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    let err = within(proxy.call_json("price", json!({"id": 1, "bogus": true})))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
    let err = within(proxy.call_json("nope", json!({})))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no method"), "{err}");
    // Wrong pattern for the typed entry points.
    let err = within(proxy.unary("explode", &DynValue::from_json(&order_json(1, &[], None))))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    // Unary methods take exactly one request.
    let err = within(proxy.call("price", vec![])).await.unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
}
