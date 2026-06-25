//! Mission Control server — serves the typed `MissionControl` service over the
//! Iroh transport AND HTTPS (H1/H2/H3) + WebTransport, from one shared
//! dispatcher.
//!
//! ```text
//! cargo run -p mission-control
//! # canonical Aster RPC, browser JSON, or WebTransport, all under /aster/...
//! #   curl --insecure -X POST https://127.0.0.1:8443/aster/MissionControl/getStatus \
//! #     -H 'content-type: application/json' -d '{"agent_id":"a1"}'
//! ```
//!
//! TLS is a generated self-signed cert (dev). For browsers, trust it or use the
//! WebTransport `serverCertificateHashes` flow (the cert hash is printed at
//! startup).

use aster::rpc::{ProjectionRegistry, Server};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_salvo::{generate_self_signed, router_with, wt_router, TlsMaterial};
use mission_control::{MissionControlImpl, MissionControlProjection, MissionControlServer};
use salvo::prelude::*;

#[tokio::main]
async fn main() -> aster::Result<()> {
    let cfg = AsterConfig::builder().relay(RelayMode::Default).build();
    let node = Node::start_with_alpns(cfg, vec![aster::rpc::RPC_ALPN.to_vec()]).await?;

    // Register once; serve the SAME dispatcher over every transport.
    let server = Server::new(&node).register(MissionControlServer::new(MissionControlImpl));
    let dispatcher = server.dispatcher();
    let _iroh = server.serve();
    println!("Iroh node id: {}", node.id());

    // One Salvo app: canonical Aster RPC + browser JSON projection (3-segment
    // /aster/{service}/{method}) and WebTransport (/aster/wt).
    let projections = ProjectionRegistry::new().register(MissionControlProjection::new());
    let app = Router::new()
        .push(router_with(dispatcher.clone(), projections))
        .push(wt_router(dispatcher));
    let service = Service::new(app);

    // Print the self-signed cert hash for WebTransport serverCertificateHashes.
    if let Ok(g) = generate_self_signed(&["localhost".into()]) {
        let hex: String = g.sha256.iter().map(|b| format!("{b:02x}")).collect();
        println!("WebTransport cert sha-256 (per-run, illustrative): {hex}");
    }

    println!("HTTPS (H1/H2/H3) + WebTransport on https://127.0.0.1:8443/aster/...");
    aster_transport_salvo::serve_https(
        "127.0.0.1:8443",
        TlsMaterial::self_signed(["localhost".into()]),
        service,
    )
    .await
    .map_err(aster::Error::Connection)?;
    Ok(())
}
