#![cfg(all(feature = "rpc", feature = "docs", feature = "blobs"))]
//! Manifest publication round-trip. A producer publishes a contract collection
//! plus registry entries into a docs namespace; it then resolves + fetches +
//! verifies the contract back through the registry, and a second node downloads
//! the collection by hash over the network and checks the embedded manifest.
//!
//! Hermetic (relay off, loopback, add_peer). The contract is the cross-binding
//! EchoService whose golden id is pinned in `rpc_contract.rs`.

use std::time::Duration;

use aster::rpc::{
    contract_id as compute_cid, fetch_and_verify_contract, publish_contract, AsterType, MethodDef,
    MethodPattern, ScopeKind, ServiceContract, TypeDef,
};
use aster::{AsterConfig, Node, RelayMode};
use tokio::time::timeout;

const GOLDEN_CID: &str = "12d2f2990f4dd71dfd59f5db470d186f1fcc7dbafdac0ea7fdf838ab263c0578";

#[derive(AsterType)]
#[aster(wire = "echo/EchoRequest")]
struct EchoRequest {
    #[allow(dead_code)]
    message: String,
}

#[derive(AsterType)]
#[aster(wire = "echo/EchoResponse")]
struct EchoResponse {
    #[allow(dead_code)]
    reply: String,
}

fn echo_contract() -> (ServiceContract, Vec<TypeDef>) {
    let sc = ServiceContract {
        name: "EchoService".into(),
        version: 1,
        methods: vec![MethodDef {
            name: "echo".into(),
            pattern: MethodPattern::Unary,
            request_type: EchoRequest::aster_type_hash_hex(),
            response_type: EchoResponse::aster_type_hash_hex(),
            idempotent: false,
            default_timeout: 0.0,
            requires: None,
        }],
        serialization_modes: vec!["xlang".into()],
        scoped: ScopeKind::Shared,
        requires: None,
        producer_language: String::new(),
    };
    let tds = vec![
        EchoRequest::aster_type_def(),
        EchoResponse::aster_type_def(),
    ];
    (sc, tds)
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

async fn within<F: std::future::Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(20), f)
        .await
        .expect("operation timed out")
}

#[tokio::test]
async fn publish_then_fetch_and_cross_node_collection() {
    let producer = Node::start(cfg()).await.unwrap();
    let consumer = Node::start(cfg()).await.unwrap();
    wait_for_addr(&producer).await;
    wait_for_addr(&consumer).await;
    consumer.add_peer(&producer).unwrap();
    producer.add_peer(&consumer).unwrap();

    let (sc, tds) = echo_contract();
    assert_eq!(compute_cid(&sc), GOLDEN_CID, "sanity: contract_id helper");

    // Publish into a fresh registry doc on the producer.
    let doc = producer.docs().create().await.unwrap();
    let author = producer.docs().default_author().await.unwrap();
    let published = within(publish_contract(
        &doc,
        &producer.blobs(),
        &author,
        &sc,
        &tds,
        1_700_000_000_000,
    ))
    .await
    .unwrap();
    assert_eq!(published.contract_id, GOLDEN_CID);

    // (a) Resolve + fetch + verify through the registry on the producer (doc is
    // local — collection is already resident, so the download is a no-op).
    let fetched = within(fetch_and_verify_contract(
        &doc,
        &producer.blobs(),
        &producer.id(),
        "EchoService",
        1,
    ))
    .await
    .unwrap(); // Ok means verified — a hash/id mismatch returns Err.
    assert_eq!(fetched.contract_id, GOLDEN_CID);
    assert_eq!(fetched.manifest.service, "EchoService");
    assert_eq!(fetched.manifest.version, 1);
    assert_eq!(fetched.manifest.type_count, 2);
    assert_eq!(fetched.manifest.method_count, 1);
    assert_eq!(fetched.manifest.methods[0].pattern, "unary");
    assert_eq!(fetched.manifest.canonical_encoding, "fory-xlang/0.15");

    // (b) Cross-node: the consumer downloads the collection by hash from the
    // producer and confirms it received the right one — `contract.bin` is
    // present and the embedded manifest carries the expected contract_id. (The
    // blake3(contract.bin) == contract_id check itself is proven by (a).)
    let entries = within(
        consumer
            .blobs()
            .download_collection(&published.collection_hash, &producer.id()),
    )
    .await
    .unwrap();
    assert!(
        entries.iter().any(|(n, _)| n == "contract.bin"),
        "downloaded collection must contain contract.bin"
    );
    let manifest_bytes = entries
        .iter()
        .find(|(n, _)| n == "manifest.json")
        .map(|(_, b)| b.clone())
        .expect("manifest.json in collection");
    let manifest =
        aster::rpc::ContractManifest::from_json(std::str::from_utf8(&manifest_bytes).unwrap())
            .expect("downloaded manifest parses");
    assert_eq!(manifest.contract_id, GOLDEN_CID);

    producer.shutdown().await;
    consumer.shutdown().await;
}
