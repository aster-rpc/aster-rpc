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
//!
//! The auth chapter: `cargo run -p mission-control -- --auth` serves the
//! service with `requires` on each method ([`mission_control::auth`]) and
//! admits only enrolled nodes. Everything comes from `AsterConfig::from_env()`
//! (`ASTER_ROOT_PUBKEY`, `ASTER_ENROLLMENT_*`, …); see the README for the CLI
//! steps that produce the files. Auth mode serves Iroh only: an HTTP caller
//! carries no enrollment credential.

use aster::rpc::{AsterServer, ProjectionRegistry};
use aster::{AsterConfig, Error, RelayMode};
use aster_transport_salvo::{generate_webtransport_cert, HttpConfig, TlsMaterial};
use mission_control::{MissionControlImpl, MissionControlProjection, MissionControlServer};

#[tokio::main]
async fn main() -> aster::Result<()> {
    let auth = std::env::args().skip(1).any(|arg| arg == "--auth");
    let config = AsterConfig::from_env()?;
    if auth {
        return serve_with_auth(config).await;
    }

    // Generate a WebTransport-valid dev cert (ECDSA, <=14 days) once so the
    // printed hash matches the served cert and browsers accept it via
    // serverCertificateHashes.
    let cert = generate_webtransport_cert("mission-control", None, &["localhost".into()])
        .map_err(Error::Connection)?;
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
        .config(config)
        .relay(RelayMode::Default)
        .with_http(http)
        .start()
        .await?;

    println!("Iroh node id: {}", srv.id());
    println!("Address: {}", srv.address()?);
    println!("WebTransport cert sha-256: {cert_hash}");
    println!("HTTPS (H1/H2/H3) + WebTransport on https://127.0.0.1:8443/aster/...");
    srv.run().await;
    Ok(())
}

/// Chapter 5: only enrolled nodes connect, and each method's `requires` is
/// checked against the roles in the caller's credential.
async fn serve_with_auth(config: AsterConfig) -> aster::Result<()> {
    if !config.is_enrollment_enabled() {
        return Err(Error::InvalidArgument(
            "--auth needs enrollment configured: ASTER_ROOT_PUBKEY, \
             ASTER_ENROLLMENT_AUDIENCE, ASTER_ENROLLMENT_FLEET_NAMESPACE, \
             ASTER_ENROLLMENT_CREDENTIAL and ASTER_ENROLLMENT_STATE_PATH \
             (see the README)"
                .into(),
        ));
    }
    let srv = AsterServer::builder()
        .service(mission_control::auth::MissionControlServer::new(
            MissionControlImpl,
        ))
        .config(config)
        .start()
        .await?;
    println!("Iroh node id: {}", srv.id());
    println!("Address: {}", srv.address()?);
    srv.run().await;
    Ok(())
}
