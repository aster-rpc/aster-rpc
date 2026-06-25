//! Mission Control server — one builder serves the typed `MissionControl`
//! service over the Iroh transport AND HTTPS (H1/H2/H3) + WebTransport, via
//! `AsterServer::builder().with_http(...)`.
//!
//! ```text
//! cargo run -p mission-control
//! #   curl --insecure -X POST https://127.0.0.1:8443/aster/MissionControl/getStatus \
//! #     -H 'content-type: application/json' -d '{"agent_id":"a1"}'
//! ```
//!
//! TLS is a generated self-signed cert (dev); its SHA-256 — the value a browser
//! pins via WebTransport `serverCertificateHashes` — is printed at startup.

use aster::rpc::{AsterServer, ProjectionRegistry};
use aster::{Error, RelayMode};
use aster_transport_salvo::{generate_self_signed, HttpConfig, TlsMaterial};
use mission_control::{MissionControlImpl, MissionControlProjection, MissionControlServer};

#[tokio::main]
async fn main() -> aster::Result<()> {
    // Generate the dev cert once so the printed hash matches the served cert.
    let cert = generate_self_signed(&["localhost".into()]).map_err(Error::Connection)?;
    let cert_hash: String = cert.sha256.iter().map(|b| format!("{b:02x}")).collect();

    let projections = ProjectionRegistry::new().register(MissionControlProjection::new());
    let http = HttpConfig::new(
        "127.0.0.1:8443",
        TlsMaterial::pem(cert.cert_pem, cert.key_pem),
    )
    .projections(projections)
    .webtransport(true);

    let srv = AsterServer::builder()
        .service(MissionControlServer::new(MissionControlImpl))
        .relay(RelayMode::Default)
        .with_http(http)
        .start()
        .await?;

    println!("Iroh node id: {}", srv.id());
    println!("WebTransport cert sha-256: {cert_hash}");
    println!("HTTPS (H1/H2/H3) + WebTransport on https://127.0.0.1:8443/aster/...");
    srv.run().await;
    Ok(())
}
