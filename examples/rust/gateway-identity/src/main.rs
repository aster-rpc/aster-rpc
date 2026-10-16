//! `cargo run -p gateway-identity`, then open <http://localhost:8080>
//! (`localhost`, not `127.0.0.1`: it is the passkeys' RP ID).
//!
//! Starts the fleet (root, stand-in identity provider, service, edge) in this
//! process and serves the edge on `127.0.0.1:8080` (`GATEWAY_EXAMPLE_PORT`
//! to change it). State lives in a temporary directory removed on exit.

use std::sync::Arc;

use gateway_identity::edge::Edge;
use gateway_identity::fleet::Fleet;
use gateway_identity::idp;
use gateway_identity::service::NotesClient;
use gateway_identity::webauthn::RelyingParty;
use salvo::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port: u16 = std::env::var("GATEWAY_EXAMPLE_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(8080);
    let state = tempfile::tempdir()?;
    println!("starting the fleet (root, identity provider, service, edge)...");
    let fleet = Fleet::start(state.path()).await?;
    println!("service: {}", fleet.service.id());
    println!("edge:    {}", fleet.edge.id());

    let notes = NotesClient::new(fleet.edge.rpc());
    let rp = RelyingParty::new("localhost", format!("http://localhost:{port}"));
    let provider = fleet.idp.clone();
    let edge = Edge::new(notes, fleet.edge.user_token_issuer()?, rp).with_sign_in(Arc::new(
        move |user: &str| provider.sign_in(user, &["readers", "writers"]),
    ));
    let router = Arc::new(edge).router();

    let acceptor = TcpListener::new(("127.0.0.1", port)).bind().await;
    println!(
        "open http://localhost:{port}  (identity provider: {})",
        idp::ISSUER
    );
    Server::new(acceptor).serve(router).await;
    Ok(())
}
