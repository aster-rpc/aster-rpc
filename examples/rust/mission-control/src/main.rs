//! Mission Control server — serves the typed `MissionControl` service over BOTH
//! the Iroh transport and HTTP (Salvo), from one shared dispatcher. HTTP is
//! plain H1/H2 on TCP here (TLS modes land in a later increment).
//!
//! ```text
//! cargo run -p mission-control
//! # then, over HTTP, POST Aster-framed bodies to
//! #   http://127.0.0.1:8080/aster/MissionControl/get_status
//! ```

use aster::rpc::Server;
use aster::{AsterConfig, Node, RelayMode};
use mission_control::{MissionControlImpl, MissionControlServer};
use salvo::conn::TcpListener;
use salvo::prelude::*;

#[tokio::main]
async fn main() -> aster::Result<()> {
    let cfg = AsterConfig::builder().relay(RelayMode::Default).build();
    let node = Node::start_with_alpns(cfg, vec![aster::rpc::RPC_ALPN.to_vec()]).await?;

    // Register once; serve the SAME dispatcher over two transports.
    let server = Server::new(&node).register(MissionControlServer::new(MissionControlImpl));
    let dispatcher = server.dispatcher();
    let _iroh = server.serve();
    println!("Iroh node id: {}", node.id());

    let service = Service::new(aster_transport_salvo::router(dispatcher));
    let acceptor = TcpListener::new("127.0.0.1:8080").bind().await;
    println!("HTTP: POST http://127.0.0.1:8080/aster/MissionControl/<method>");
    salvo::Server::new(acceptor).serve(service).await;
    Ok(())
}
