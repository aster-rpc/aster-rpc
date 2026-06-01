#![cfg(feature = "rpc")]
//! `#[aster::service]` with all four call patterns (unary + the three streaming
//! kinds declared via `#[rpc(server_stream|client_stream|bidi_stream)]`),
//! round-tripped end-to-end through the generated server + client.

use std::future::Future;
use std::time::Duration;

use aster::rpc::{async_trait, RequestStream, ResponseSink, RpcConnection, Server, RPC_ALPN};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use tokio::time::timeout;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "streams/Msg")]
struct Msg {
    text: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "streams/Count")]
struct Count {
    n: i32,
}

#[aster::service(name = "Streams", version = 1)]
trait Streams {
    async fn unary_echo(&self, req: Msg) -> Result<Msg>;

    #[rpc(server_stream)]
    async fn count_up(&self, req: Count, out: ResponseSink<Count>) -> Result<()>;

    #[rpc(client_stream)]
    async fn sum(&self, reqs: RequestStream<Count>) -> Result<Count>;

    #[rpc(bidi_stream)]
    async fn double(&self, reqs: RequestStream<Count>, out: ResponseSink<Count>) -> Result<()>;
}

struct StreamsImpl;

#[async_trait]
impl Streams for StreamsImpl {
    async fn unary_echo(&self, req: Msg) -> Result<Msg> {
        Ok(Msg {
            text: format!("echo:{}", req.text),
        })
    }

    async fn count_up(&self, req: Count, out: ResponseSink<Count>) -> Result<()> {
        for i in 0..req.n {
            out.send(&Count { n: i })?;
        }
        Ok(())
    }

    async fn sum(&self, mut reqs: RequestStream<Count>) -> Result<Count> {
        let mut total = 0;
        while let Some(c) = reqs.recv().await? {
            total += c.n;
        }
        Ok(Count { n: total })
    }

    async fn double(&self, mut reqs: RequestStream<Count>, out: ResponseSink<Count>) -> Result<()> {
        while let Some(c) = reqs.recv().await? {
            out.send(&Count { n: c.n * 2 })?;
        }
        Ok(())
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
async fn all_four_patterns_through_macro() {
    // The generated contract records each method's pattern.
    let contract = StreamsClient::contract();
    let pattern = |m: &str| {
        format!(
            "{:?}",
            contract
                .methods
                .iter()
                .find(|x| x.name == m)
                .unwrap()
                .pattern
        )
    };
    assert_eq!(pattern("unary_echo"), "Unary");
    assert_eq!(pattern("count_up"), "ServerStream");
    assert_eq!(pattern("sum"), "ClientStream");
    assert_eq!(pattern("double"), "BidiStream");

    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client_node = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client_node).await;
    client_node.add_peer(&server).unwrap();
    server.add_peer(&client_node).unwrap();

    let _h = Server::new(&server)
        .register(StreamsServer::new(StreamsImpl))
        .serve();

    let conn: RpcConnection = within(client_node.rpc_connect(&server.id())).await.unwrap();
    let client = StreamsClient::new(conn);

    // unary
    let resp = within(client.unary_echo(Msg { text: "hi".into() }))
        .await
        .unwrap();
    assert_eq!(resp.text, "echo:hi");

    // server-stream
    let mut stream = within(client.count_up(Count { n: 3 })).await.unwrap();
    let got = within(stream.collect()).await.unwrap();
    assert_eq!(got, vec![Count { n: 0 }, Count { n: 1 }, Count { n: 2 }]);

    // client-stream
    let total = within(client.sum(vec![Count { n: 1 }, Count { n: 2 }, Count { n: 3 }]))
        .await
        .unwrap();
    assert_eq!(total, Count { n: 6 });

    // bidi
    let mut stream = within(client.double(vec![Count { n: 2 }, Count { n: 5 }]))
        .await
        .unwrap();
    let got = within(stream.collect()).await.unwrap();
    assert_eq!(got, vec![Count { n: 4 }, Count { n: 10 }]);

    client_node.shutdown().await;
    server.shutdown().await;
}
