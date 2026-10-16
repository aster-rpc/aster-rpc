//! A self-hosted relay whose certificate comes from a private CA (Kasm A6).
//!
//! An in-process iroh relay serves HTTPS with a leaf certificate signed by a
//! throwaway CA. Two nodes use it as their only path (IP transports cleared,
//! discovery `none`) and dial each other by full address (node ID plus relay
//! URL). With the CA in `relay_ca_cert_files` they reach the relay and connect
//! through it; without it the relay's certificate is rejected
//! (`UnknownIssuer`) and the node never gets a home relay.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aster_transport_core::network_config::relay_ca_tls_config;
use aster_transport_core::{CoreEndpointConfig, CoreNetClient};
use iroh::{EndpointAddr, RelayUrl, TransportAddr};
use iroh_relay::server::{CertConfig, RelayConfig, Server, ServerConfig, TlsConfig};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};

const ALPN: &[u8] = b"aster.test/relay-private-ca/1";
/// Generous for loopback; the positive path comes online in well under 1 s.
const ONLINE_BUDGET: Duration = Duration::from_secs(20);
/// How long a node without the CA is given to (wrongly) come online.
const REJECT_WINDOW: Duration = Duration::from_secs(6);

struct Pki {
    dir: PathBuf,
    ca_pem: PathBuf,
    server_config: Option<rustls::ServerConfig>,
}

impl Drop for Pki {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A private CA and a relay leaf for 127.0.0.1 / localhost signed by it.
fn private_pki() -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Aster test private relay CA");
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let leaf_key = KeyPair::generate().unwrap();
    let leaf_params =
        CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()]).unwrap();
    let leaf = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

    let server_config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone(), ca_cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        )
        .unwrap();

    let dir = std::env::temp_dir().join(format!(
        "aster-relay-private-ca-{}-{:x}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let ca_pem = dir.join("ca.pem");
    std::fs::write(&ca_pem, ca_cert.pem()).unwrap();
    Pki {
        dir,
        ca_pem,
        server_config: Some(server_config),
    }
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64
}

async fn spawn_relay(pki: &mut Pki) -> (Server, RelayUrl, SocketAddr) {
    let server_config = pki.server_config.take().expect("one relay per PKI");
    let mut relay = RelayConfig::new((Ipv4Addr::LOCALHOST, 0));
    relay.tls = Some(TlsConfig::new(
        (Ipv4Addr::LOCALHOST, 0),
        CertConfig::Manual { server_config },
    ));
    let mut config = ServerConfig::default();
    config.relay = Some(relay);
    let server = Server::spawn(config).await.expect("relay starts");
    let https = server.https_addr().expect("https configured");
    let url: RelayUrl = format!("https://{https}").parse().unwrap();
    (server, url, https)
}

/// A relay-only node with discovery off, trusting `ca` for relay TLS.
fn node_config(relay: &RelayUrl, ca: Option<&Path>, ca_only: bool) -> CoreEndpointConfig {
    CoreEndpointConfig {
        relay_mode: Some("custom".into()),
        relay_urls: vec![relay.to_string()],
        alpns: vec![ALPN.to_vec()],
        clear_ip_transports: true,
        discovery: Some("none".into()),
        relay_ca_cert_files: ca
            .map(|p| vec![p.to_string_lossy().into_owned()])
            .unwrap_or_default(),
        relay_ca_only: ca_only,
        portmapper_config: Some("disabled".into()),
        ..Default::default()
    }
}

async fn online_node(config: CoreEndpointConfig) -> CoreNetClient {
    tokio::time::timeout(ONLINE_BUDGET, CoreNetClient::create_with_config(config))
        .await
        .expect("node reaches the private-CA relay")
        .expect("node starts")
}

/// One TLS handshake against the relay's HTTPS port with the client config
/// the node builds from `config`.
fn relay_handshake(config: &CoreEndpointConfig, https: SocketAddr) -> Result<(), String> {
    let ca = relay_ca_tls_config(config)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let client = ca.client_config(provider()).map_err(|e| e.to_string())?;
    let name = ServerName::IpAddress(https.ip().into());
    let mut conn = rustls::ClientConnection::new(Arc::new(client), name).unwrap();
    let mut sock = TcpStream::connect(https).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
    tls.write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut reply = Vec::new();
    let _ = tls.read_to_end(&mut reply);
    if reply.starts_with(b"HTTP/1.1 200") {
        Ok(())
    } else {
        Err(format!(
            "unexpected reply: {}",
            String::from_utf8_lossy(&reply)
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_with_private_ca_connects_only_with_the_ca_configured() {
    let mut pki = private_pki();
    let (_relay, url, https) = spawn_relay(&mut pki).await;

    // The trust decision itself, at the TLS layer.
    let with_ca = node_config(&url, Some(&pki.ca_pem), false);
    let ca_only = node_config(&url, Some(&pki.ca_pem), true);
    let without = node_config(&url, None, false);
    tokio::task::spawn_blocking({
        let (with_ca, ca_only, without) = (with_ca.clone(), ca_only.clone(), without.clone());
        move || {
            relay_handshake(&with_ca, https).expect("WebPKI + private CA accepts the relay");
            relay_handshake(&ca_only, https).expect("private CA alone accepts the relay");
            let err = relay_handshake(&without, https).expect_err("WebPKI alone rejects it");
            assert!(err.contains("UnknownIssuer"), "{err}");
        }
    })
    .await
    .unwrap();

    // Without the CA a node never gets its home relay, so it never comes online.
    let rejected =
        tokio::time::timeout(REJECT_WINDOW, CoreNetClient::create_with_config(without)).await;
    assert!(
        rejected.is_err(),
        "a node without relay_ca_certs must not come online through the private-CA relay"
    );

    // With it, two relay-only nodes connect by full address through the relay.
    let a = online_node(with_ca).await;
    let b = online_node(ca_only).await;
    assert!(a.endpoint.bound_sockets().is_empty(), "relay-only");
    assert_eq!(
        a.endpoint.address_lookup().unwrap().len(),
        0,
        "discovery none installs no lookup service"
    );

    let server = a.endpoint.clone();
    let echo = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let msg = recv.read_to_end(64).await.unwrap();
        send.write_all(&msg).await.unwrap();
        send.finish().unwrap();
        conn.closed().await;
    });

    let a_addr = EndpointAddr::from_parts(a.endpoint.id(), [TransportAddr::Relay(url.clone())]);
    let conn = tokio::time::timeout(ONLINE_BUDGET, b.endpoint.connect(a_addr, ALPN))
        .await
        .expect("dial within budget")
        .expect("dial through the private-CA relay");
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(b"ping").await.unwrap();
    send.finish().unwrap();
    let reply = recv.read_to_end(64).await.unwrap();
    assert_eq!(reply, b"ping");
    conn.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(Duration::from_secs(5), echo).await;
}
