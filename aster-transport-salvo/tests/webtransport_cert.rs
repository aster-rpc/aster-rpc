//! WebTransport `serverCertificateHashes` flow: the server presents a
//! node-bound self-signed cert (ECDSA, ≤14 days); the client connects by
//! **pinning the cert hash** (no CA, no cert-validation bypass) — exactly what a
//! browser does. Proves `generate_webtransport_cert` + the hash pinning path.

use std::time::Duration;

use aster::rpc::codec::{decode_rpc_status, encode_stream_header, SerializationMode, StreamHeader};
use aster::rpc::{Call, RpcStatus, Server, ServiceDispatch};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{
    decode_frame, encode_frame, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER,
};
use aster_transport_salvo::{generate_webtransport_cert, serve_https, wt_router, TlsMaterial};
use salvo::prelude::*;
use tokio::io::AsyncReadExt;
use wtransport::tls::Sha256Digest;
use wtransport::{ClientConfig, Endpoint};

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

#[tokio::test]
async fn webtransport_server_certificate_hashes() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .build(),
    )
    .await
    .unwrap();
    let dispatcher = Server::new(&node).register(Echo).dispatcher();
    let service = Service::new(wt_router(dispatcher));

    // Node-bound WT cert; the client will pin its hash.
    let cert = generate_webtransport_cert(&node.id().to_string(), &["localhost".into()]).unwrap();
    let cert_hash = cert.sha256;
    let tls = TlsMaterial::pem(cert.cert_pem, cert.key_pem);

    let port = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let addr = format!("127.0.0.1:{port}");
    tokio::spawn(async move {
        let _ = serve_https(&addr, tls, service).await;
    });

    // Pin the cert hash — no cert-validation bypass, no CA.
    let client = Endpoint::client(
        ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([Sha256Digest::new(cert_hash)])
            .build(),
    )
    .unwrap();

    let url = format!("https://127.0.0.1:{port}/aster/wt");
    let mut conn = None;
    for _ in 0..40 {
        match client.connect(&url).await {
            Ok(c) => {
                conn = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let conn = conn.expect("WebTransport (hash-pinned) did not connect");

    let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();
    let hdr = encode_stream_header(&StreamHeader {
        service: "Echo".into(),
        method: "unary".into(),
        version: 1,
        call_id: 0,
        deadline: 0,
        serialization_mode: SerializationMode::Xlang.as_i8(),
        metadata_keys: vec![],
        metadata_values: vec![],
        session_id: 0,
    })
    .unwrap();
    send.write_all(&encode_frame(&hdr, FLAG_HEADER).unwrap())
        .await
        .unwrap();
    send.write_all(&encode_frame(&[1, 2, 3], FLAG_END_STREAM).unwrap())
        .await
        .unwrap();

    let mut bytes = Vec::new();
    recv.read_to_end(&mut bytes).await.unwrap();
    let (payload, _f, consumed) = decode_frame(&bytes).unwrap();
    assert_eq!(payload, vec![3, 2, 1]);
    let (trailer, tflags, _) = decode_frame(&bytes[consumed..]).unwrap();
    assert!(tflags & FLAG_TRAILER != 0);
    assert_eq!(decode_rpc_status(&trailer).unwrap().code, 0);

    node.shutdown().await;
}
