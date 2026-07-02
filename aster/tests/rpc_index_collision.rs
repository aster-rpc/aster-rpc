#![cfg(feature = "rpc")]
//! Regression test for cross-crate Fory type-index collisions (TradeStorm).
//!
//! Fory 1.3 gives every `#[derive(ForyStruct)]` type a `fory_type_index()`
//! allocated from a **per-crate** counter that restarts at 0 in each crate.
//! Registration claims `type_id_index[fory_type_index()]` and errors when the
//! slot is taken — so two types from *different* crates (e.g. a user payload
//! type and `aster::rpc::Empty`) can collide inside one Fory runtime even
//! though both are registered by wire name.
//!
//! The fix keeps each RPC payload root type (each method's request/response)
//! in its own Fory runtime, so a service mixing user types with aster-crate
//! types (the implicit `Empty` of a no-request method) can no longer collide
//! across payload roots. A collision *within* one root's field tree is
//! irreducible; `PayloadRegistry` reports it with both wire names.

use std::time::Duration;

use aster::rpc::{async_trait, RpcConnection, Server, RPC_ALPN};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_core::StructSerializer;
use fory_derive::ForyStruct;

// 32 payload types. This is a dedicated test crate, so they take Fory type
// indices 0..=31 in declaration order — guaranteed to cover the index of
// every aster-crate payload type (asserted below) so that the implicit
// `Empty` request of `ping()` collides with one of them in a shared runtime.
macro_rules! payloads {
    ($($t:ident),* $(,)?) => {
        $(
            #[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
            struct $t {
                v: i32,
            }
        )*
    };
}

payloads!(
    P00, P01, P02, P03, P04, P05, P06, P07, P08, P09, P10, P11, P12, P13, P14, P15, P16, P17, P18,
    P19, P20, P21, P22, P23, P24, P25, P26, P27, P28, P29, P30, P31,
);

#[aster::service(name = "Collide", version = 1)]
trait Collide {
    async fn m00(&self, req: P00) -> Result<P00>;
    async fn m01(&self, req: P01) -> Result<P01>;
    async fn m02(&self, req: P02) -> Result<P02>;
    async fn m03(&self, req: P03) -> Result<P03>;
    async fn m04(&self, req: P04) -> Result<P04>;
    async fn m05(&self, req: P05) -> Result<P05>;
    async fn m06(&self, req: P06) -> Result<P06>;
    async fn m07(&self, req: P07) -> Result<P07>;
    async fn m08(&self, req: P08) -> Result<P08>;
    async fn m09(&self, req: P09) -> Result<P09>;
    async fn m10(&self, req: P10) -> Result<P10>;
    async fn m11(&self, req: P11) -> Result<P11>;
    async fn m12(&self, req: P12) -> Result<P12>;
    async fn m13(&self, req: P13) -> Result<P13>;
    async fn m14(&self, req: P14) -> Result<P14>;
    async fn m15(&self, req: P15) -> Result<P15>;
    async fn m16(&self, req: P16) -> Result<P16>;
    async fn m17(&self, req: P17) -> Result<P17>;
    async fn m18(&self, req: P18) -> Result<P18>;
    async fn m19(&self, req: P19) -> Result<P19>;
    async fn m20(&self, req: P20) -> Result<P20>;
    async fn m21(&self, req: P21) -> Result<P21>;
    async fn m22(&self, req: P22) -> Result<P22>;
    async fn m23(&self, req: P23) -> Result<P23>;
    async fn m24(&self, req: P24) -> Result<P24>;
    async fn m25(&self, req: P25) -> Result<P25>;
    async fn m26(&self, req: P26) -> Result<P26>;
    async fn m27(&self, req: P27) -> Result<P27>;
    async fn m28(&self, req: P28) -> Result<P28>;
    async fn m29(&self, req: P29) -> Result<P29>;
    async fn m30(&self, req: P30) -> Result<P30>;
    async fn m31(&self, req: P31) -> Result<P31>;
    // Implicit `Empty` request — an aster-crate ForyStruct whose per-crate
    // index necessarily equals one of P00..P31's.
    async fn ping(&self) -> Result<P00>;
}

struct CollideImpl;

#[async_trait]
impl Collide for CollideImpl {
    async fn m00(&self, req: P00) -> Result<P00> {
        Ok(req)
    }
    async fn m01(&self, req: P01) -> Result<P01> {
        Ok(req)
    }
    async fn m02(&self, req: P02) -> Result<P02> {
        Ok(req)
    }
    async fn m03(&self, req: P03) -> Result<P03> {
        Ok(req)
    }
    async fn m04(&self, req: P04) -> Result<P04> {
        Ok(req)
    }
    async fn m05(&self, req: P05) -> Result<P05> {
        Ok(req)
    }
    async fn m06(&self, req: P06) -> Result<P06> {
        Ok(req)
    }
    async fn m07(&self, req: P07) -> Result<P07> {
        Ok(req)
    }
    async fn m08(&self, req: P08) -> Result<P08> {
        Ok(req)
    }
    async fn m09(&self, req: P09) -> Result<P09> {
        Ok(req)
    }
    async fn m10(&self, req: P10) -> Result<P10> {
        Ok(req)
    }
    async fn m11(&self, req: P11) -> Result<P11> {
        Ok(req)
    }
    async fn m12(&self, req: P12) -> Result<P12> {
        Ok(req)
    }
    async fn m13(&self, req: P13) -> Result<P13> {
        Ok(req)
    }
    async fn m14(&self, req: P14) -> Result<P14> {
        Ok(req)
    }
    async fn m15(&self, req: P15) -> Result<P15> {
        Ok(req)
    }
    async fn m16(&self, req: P16) -> Result<P16> {
        Ok(req)
    }
    async fn m17(&self, req: P17) -> Result<P17> {
        Ok(req)
    }
    async fn m18(&self, req: P18) -> Result<P18> {
        Ok(req)
    }
    async fn m19(&self, req: P19) -> Result<P19> {
        Ok(req)
    }
    async fn m20(&self, req: P20) -> Result<P20> {
        Ok(req)
    }
    async fn m21(&self, req: P21) -> Result<P21> {
        Ok(req)
    }
    async fn m22(&self, req: P22) -> Result<P22> {
        Ok(req)
    }
    async fn m23(&self, req: P23) -> Result<P23> {
        Ok(req)
    }
    async fn m24(&self, req: P24) -> Result<P24> {
        Ok(req)
    }
    async fn m25(&self, req: P25) -> Result<P25> {
        Ok(req)
    }
    async fn m26(&self, req: P26) -> Result<P26> {
        Ok(req)
    }
    async fn m27(&self, req: P27) -> Result<P27> {
        Ok(req)
    }
    async fn m28(&self, req: P28) -> Result<P28> {
        Ok(req)
    }
    async fn m29(&self, req: P29) -> Result<P29> {
        Ok(req)
    }
    async fn m30(&self, req: P30) -> Result<P30> {
        Ok(req)
    }
    async fn m31(&self, req: P31) -> Result<P31> {
        Ok(req)
    }
    async fn ping(&self) -> Result<P00> {
        Ok(P00 { v: -1 })
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

/// Precondition for the tests below: `Empty`'s per-crate index falls inside
/// the P00..P31 range, i.e. the collision scenario is actually present. If the
/// aster crate ever grows past 32 `ForyStruct` derives before `Empty`, widen
/// the payload set above.
#[test]
fn empty_index_within_test_payload_range() {
    let e = <aster::rpc::Empty as StructSerializer>::fory_type_index();
    assert!(
        e < 32,
        "aster::rpc::Empty has fory_type_index {e}; widen P00..P31 so the \
         collision scenario stays covered"
    );
}

/// TradeStorm regression: constructing a server whose payload runtime mixes
/// user-crate types with the aster-crate `Empty` must not panic with Fory's
/// "Type index N already registered".
#[tokio::test]
async fn server_with_cross_crate_index_overlap_starts_and_serves() {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client_node = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client_node).await;
    client_node.add_peer(&server).unwrap();
    server.add_peer(&client_node).unwrap();

    // Panicked here before the per-root payload runtimes fix.
    let _h = Server::new(&server)
        .register(CollideServer::new(CollideImpl))
        .serve();

    let conn: RpcConnection = client_node.rpc_connect(&server.id()).await.unwrap();
    let client = CollideClient::new(conn);

    // Round-trip a colliding-index type and the implicit-Empty method.
    let out = client.m10(P10 { v: 42 }).await.unwrap();
    assert_eq!(out.v, 42);
    let pong = client.ping().await.unwrap();
    assert_eq!(pong.v, -1);

    client_node.shutdown().await;
    server.shutdown().await;
}

/// The irreducible case: a collision *inside a single payload root's tree*
/// (here forced by registering a colliding pair into one `PayloadRegistry`)
/// must fail with a diagnostic naming both wire types, not Fory's bare
/// "Type index N already registered".
#[test]
fn intra_tree_collision_names_both_types() {
    use aster::rpc::{PayloadRegistry, WireField};

    let e = <aster::rpc::Empty as StructSerializer>::fory_type_index();
    let mut reg = PayloadRegistry::new();
    <aster::rpc::Empty as WireField>::register_payload(&mut reg);

    macro_rules! register_matching {
        ($($i:literal => $t:ident),* $(,)?) => {
            match e {
                $($i => <$t as WireField>::register_payload(&mut reg),)*
                _ => unreachable!("guarded by empty_index_within_test_payload_range"),
            }
        };
    }
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        register_matching!(
            0 => P00, 1 => P01, 2 => P02, 3 => P03, 4 => P04, 5 => P05, 6 => P06, 7 => P07,
            8 => P08, 9 => P09, 10 => P10, 11 => P11, 12 => P12, 13 => P13, 14 => P14,
            15 => P15, 16 => P16, 17 => P17, 18 => P18, 19 => P19, 20 => P20, 21 => P21,
            22 => P22, 23 => P23, 24 => P24, 25 => P25, 26 => P26, 27 => P27, 28 => P28,
            29 => P29, 30 => P30, 31 => P31,
        );
    }))
    .unwrap_err();
    let msg = panic
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| panic.downcast_ref::<&str>().unwrap_or(&"").to_string());
    assert!(
        msg.contains("type-index collision") && msg.contains("aster.Empty"),
        "diagnostic should name the colliding wire types, got: {msg}"
    );
}
