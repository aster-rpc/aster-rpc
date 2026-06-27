//! Real HTTPS handshake: a self-signed server + an external rustls client
//! (reqwest) doing a canonical Aster-frames POST over TLS. Proves TLS end to
//! end (the in-process TestClient can't exercise the transport). Also unit-tests
//! self-signed cert generation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{Call, RpcStatus, Server, ServiceDispatch};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, encode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use aster_transport_salvo::{generate_self_signed, rustls_config, TlsMaterial};
use salvo::conn::TcpListener;
use salvo::prelude::*;

struct Echo;

#[aster::rpc::async_trait]
impl ServiceDispatch for Echo {
    fn name(&self) -> &str {
        "Echo"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["unary"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let req = call.recv_request().await.unwrap_or_default();
        let resp: Vec<u8> = req.into_iter().rev().collect();
        let _ = call.respond(resp, &RpcStatus::ok());
    }
}

#[test]
fn self_signed_generation() {
    let g = generate_self_signed(None, &["localhost".into()]).unwrap();
    let cert = String::from_utf8(g.cert_pem.clone()).unwrap();
    let key = String::from_utf8(g.key_pem.clone()).unwrap();
    assert!(cert.contains("BEGIN CERTIFICATE"));
    assert!(key.contains("PRIVATE KEY"));
    assert_eq!(g.sha256.len(), 32);
    // A static config builds from the generated PEM.
    rustls_config(&TlsMaterial::Pem {
        cert_pem: g.cert_pem,
        key_pem: g.key_pem,
    })
    .unwrap();
}

/// `Some(node_id)` stamps an `aster://<node_id>` URI SAN that binds the cert to
/// the Aster identity; `None` leaves it off. Parse the DER and check the SAN.
#[test]
fn self_signed_binds_aster_identity_via_uri_san() {
    use x509_parser::extensions::GeneralName;
    use x509_parser::prelude::FromDer;

    fn uri_sans(cert_pem: &[u8]) -> Vec<String> {
        let der = pem::parse(cert_pem).unwrap();
        let (_, cert) =
            x509_parser::certificate::X509Certificate::from_der(der.contents()).unwrap();
        cert.subject_alternative_name()
            .unwrap()
            .map(|san| {
                san.value
                    .general_names
                    .iter()
                    .filter_map(|n| match n {
                        GeneralName::URI(u) => Some((*u).to_string()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    let node_id = "a".repeat(64); // shape of an ed25519 node id (hex)
    let bound = generate_self_signed(
        Some(aster_transport_salvo::NodeBinding::Claim(&node_id)),
        &["localhost".into()],
    )
    .unwrap();
    assert!(
        uri_sans(&bound.cert_pem).contains(&format!("aster://{node_id}")),
        "expected aster:// URI SAN on the node-bound cert"
    );

    let plain = generate_self_signed(None, &["localhost".into()]).unwrap();
    assert!(
        uri_sans(&plain.cert_pem).is_empty(),
        "no URI SAN expected without a node id"
    );
}

/// A `Signed` binding is cryptographically verifiable: the node key signed the
/// cert's public key, so `verify_cert_binding` accepts it against the right node
/// id and rejects a MITM (a cert signed by some *other* key) and an unsigned
/// claim. This is the property that defeats cert substitution.
#[test]
fn signed_binding_verifies_and_rejects_substitution() {
    use aster_transport_salvo::{verify_cert_binding, NodeBinding};
    use ed25519_dalek::SigningKey;

    let node_secret = [7u8; 32];
    let node_id = hex::encode(
        SigningKey::from_bytes(&node_secret)
            .verifying_key()
            .to_bytes(),
    );

    // The node signs its own cert → verifies against its id.
    let cert = generate_self_signed(
        Some(NodeBinding::Signed(&node_secret)),
        &["localhost".into()],
    )
    .unwrap();
    assert!(verify_cert_binding(&cert.cert_pem, &node_id).is_ok());

    // Verifying against a different id is rejected.
    let other = hex::encode(
        SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .to_bytes(),
    );
    assert!(verify_cert_binding(&cert.cert_pem, &other).is_err());

    // MITM: an attacker mints its own signed cert. It can only bind to the
    // attacker's id (the signature is over the attacker's key), so verifying
    // against the victim id fails — the attacker can't forge the victim's sig.
    let mitm =
        generate_self_signed(Some(NodeBinding::Signed(&[3u8; 32])), &["localhost".into()]).unwrap();
    assert!(verify_cert_binding(&mitm.cert_pem, &node_id).is_err());

    // A claim-only cert carries no signature → not accepted as proof.
    let claim =
        generate_self_signed(Some(NodeBinding::Claim(&node_id)), &["localhost".into()]).unwrap();
    assert!(verify_cert_binding(&claim.cert_pem, &node_id).is_err());
}

#[tokio::test]
async fn https_self_signed_roundtrip() {
    // Two rustls stacks are linked (salvo + reqwest), so rustls can't pick a
    // default crypto provider on its own — install one. Not needed in prod.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();
    let dispatcher = Server::new(&node).register(Echo).dispatcher();
    let service = Service::new(aster_transport_salvo::router(dispatcher));

    // Grab a free port, then bind a real TLS listener on it.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let config = rustls_config(&TlsMaterial::self_signed(["localhost".into()])).unwrap();
    let acceptor = TcpListener::new(format!("127.0.0.1:{port}"))
        .rustls(config)
        .bind()
        .await;
    tokio::spawn(async move { salvo::Server::new(acceptor).serve(service).await });

    // External rustls client; accept the self-signed cert.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let url = format!("https://127.0.0.1:{port}/aster/Echo/unary");
    let body = encode_frame(&[1, 2, 3], FLAG_END_STREAM).unwrap();

    // Retry while the spawned server comes up.
    let mut resp = None;
    for _ in 0..40 {
        match client
            .post(&url)
            .header("content-type", "application/aster-frames")
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let resp = resp.expect("HTTPS server did not come up");
    assert!(resp.status().is_success(), "status {}", resp.status());
    let bytes = resp.bytes().await.unwrap();

    // Decode the response frames: data [3,2,1] then OK trailer.
    let (payload, _f, consumed) = decode_frame(&bytes).unwrap();
    assert_eq!(payload, vec![3, 2, 1]);
    let (trailer, tflags, _) = decode_frame(&bytes[consumed..]).unwrap();
    assert!(tflags & FLAG_TRAILER != 0);
    assert_eq!(decode_rpc_status(&trailer).unwrap().code, 0);

    node.shutdown().await;
}

/// `serve_https_with` threads the tuner down to the H3 `QuinnListener`, which
/// runs it (against the real re-exported `TransportConfig`) when it binds.
#[tokio::test]
async fn serve_https_with_invokes_transport_tuner() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();
    let dispatcher = Server::new(&node).register(Echo).dispatcher();
    let service = Service::new(aster_transport_salvo::router(dispatcher));

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };

    let tuned = Arc::new(AtomicBool::new(false));
    let tuned_in = tuned.clone();
    let server = tokio::spawn(async move {
        let _ = aster_transport_salvo::serve_https_with(
            &format!("127.0.0.1:{port}"),
            TlsMaterial::self_signed(["localhost".into()]),
            service,
            move |t: &mut aster_transport_salvo::TransportConfig| {
                // Names the re-exported type and mutates it like Portal will.
                t.send_window(256 * 1024);
                tuned_in.store(true, Ordering::SeqCst);
            },
        )
        .await;
    });

    // The tuner fires when the QuinnListener builds its initial config (at bind),
    // before the serve loop — poll the flag while the spawned server comes up.
    let mut invoked = false;
    for _ in 0..40 {
        if tuned.load(Ordering::SeqCst) {
            invoked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.abort();
    node.shutdown().await;
    assert!(
        invoked,
        "serve_https_with did not invoke the transport tuner"
    );
}
