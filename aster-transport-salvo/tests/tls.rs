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
    let g = generate_self_signed(&["localhost".into()]).unwrap();
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
